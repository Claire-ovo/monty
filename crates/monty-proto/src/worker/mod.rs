//! Child-side protocol state machine: the transport-agnostic core of a monty
//! worker.
//!
//! [`Child`] holds one REPL session and serves [`pb::ParentRequest`]s in strict
//! alternation (one request in, zero or more `Print` events out, then exactly
//! one turn-ending event). It is generic over an [`Engine`] — the language
//! runtime that creates, feeds, suspends and resumes sessions — so the wire
//! contract (dispatch, state transitions, call-id validation, framing, budget
//! stamping, print pacing) is written once for every language Monty runs.
//! `python` holds the Python interpreter's engine.
//!
//! Transport shells wrap it: `monty subprocess` over framed stdio, the wasm
//! worker over `postMessage`, and the in-process `dispatch_frame` used by tests.
//!
//! Values cross as `MontyGraph` arenas and exceptions as `MontyException`,
//! both from `monty-types`, so the child itself links no interpreter: an
//! engine's crate enables the `worker` feature and implements the trait.

use std::{mem, time::Duration};

use monty_types::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, NamedValues, OsPolicy, PrintWriter, ResourceLimits,
    ResourceTracker,
};

use super::{
    BudgetVec, DEFAULT_PRINT_FLUSH_INTERVAL, FrameError, FrameReader, MAX_FRAME_LEN, ProtoConvertError,
    WireFunctionCall, check_protocol_version, exceeds_max_frame_len, ext_result_from_proto, future_results_from_proto,
    named_values_from_proto, os_call_from_proto, os_call_to_proto, pb, write_frame,
};
use crate::{convert::limits::micros_field, wire::uuid_to_pb};

mod engine;
mod print;
#[cfg(feature = "worker-python")]
pub mod python;

pub use engine::{Engine, Failed, Loaded, LoadedSession, Outcome, PreFeed, SessionRef, Step, Suspension};
use print::ProtoPrint;

/// A sink for framed [`pb::ChildEvent`]s, decoupling the child from its
/// transport.
///
/// The native subprocess implements this over stdout; the wasm worker buffers
/// frames for the host to read (see [`VecEventSink`]). `send` frames the event
/// (4-byte LE length prefix + protobuf) exactly as `monty-proto`'s
/// [`write_frame`] does.
///
/// `Err` is a transport failure the caller treats as terminal: a broken pipe
/// (the parent is gone) for stdout, or — for an in-memory buffer that cannot
/// fail on I/O — only an oversize frame, which [`write_frame`] rejects *before*
/// buffering any bytes, so the stream stays in sync and the child can recover.
pub trait EventSink {
    /// Frames and emits one event.
    fn send(&mut self, event: &pb::ChildEvent) -> Result<(), FrameError>;
}

/// An [`EventSink`] that appends framed events to an in-memory buffer.
///
/// Used by the wasm worker, which collects a turn's frames and hands the whole
/// buffer back to the host in one `postMessage`, and by tests that drive
/// [`Child`] in-process. [`Self::take`] yields the accumulated frames and
/// resets the buffer for the next turn.
#[derive(Default)]
pub struct VecEventSink {
    frames: Vec<u8>,
}

impl VecEventSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the frames buffered since the last call and clears the buffer.
    pub fn take(&mut self) -> Vec<u8> {
        mem::take(&mut self.frames)
    }
}

impl EventSink for VecEventSink {
    fn send(&mut self, event: &pb::ChildEvent) -> Result<(), FrameError> {
        // `Vec<u8>: io::Write` never fails on I/O, so the only error this can
        // surface is `FrameTooLarge`, which `write_frame` raises before
        // appending anything.
        write_frame(&mut self.frames, event)
    }
}

/// What the host loop should do after [`Child::handle`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleOutcome {
    /// Keep serving the next request.
    Continue,
    /// The child received `Shutdown` and should exit cleanly.
    Shutdown,
    /// The child emitted a `FatalError` (e.g. parent/child version skew) and
    /// must terminate. Distinct from `Shutdown` so a native host can exit with
    /// a non-zero status; a message-based host treats it like `Shutdown`.
    Fatal,
}

/// Runs one buffered turn: reads exactly one framed `ParentRequest` from
/// `request_frame`, handles it on `child`, and returns the concatenated framed
/// events (zero or more `Print`s then one turn-ending event) plus what the host
/// should do next.
///
/// This is the per-turn entry point for a message-based transport such as a
/// wasm Web Worker, where each `postMessage` carries one request frame and the
/// reply carries that turn's frames. It mirrors the native shell's stdio loop
/// body — malformed frames become a `protocol_violation` (recoverable) or a
/// `FatalError` (desync), and an unrecoverable oversize event becomes a
/// `FatalError` — but writes to an in-memory buffer instead of a pipe.
///
/// Unlike a streaming transport, `Print` events are buffered for the whole turn
/// and returned together rather than delivered incrementally.
pub fn dispatch_frame<E: Engine>(child: &mut Child<E>, request_frame: &[u8]) -> (Vec<u8>, HandleOutcome) {
    let mut sink = VecEventSink::new();
    let outcome = dispatch_into(child, request_frame, &mut sink);
    (sink.take(), outcome)
}

/// Decodes and handles one request frame, sending all resulting frames to
/// `sink`. Factored out of [`dispatch_frame`] so the framing/recovery decisions
/// stay separate from buffer ownership.
fn dispatch_into<E: Engine>(child: &mut Child<E>, request_frame: &[u8], sink: &mut VecEventSink) -> HandleOutcome {
    let mut reader = FrameReader::new(request_frame);
    match reader.read::<pb::ParentRequest>() {
        Ok(Some(request)) => match child.handle(request, sink) {
            Ok(outcome) => outcome,
            // an oversize turn-ending event was rejected before any bytes were
            // buffered, so the reply is still parseable — but an oversize
            // suspension (or any unrecoverable error) leaves no resume point,
            // so emit a fatal last gasp and stop the worker
            Err(FrameError::FrameTooLarge { len, max }) => {
                let _ = sink
                    .send(&child.fatal_event(&format!("response frame of {len} bytes exceeds maximum of {max} bytes")));
                HandleOutcome::Shutdown
            }
            // `VecEventSink` cannot fail on I/O, so this is unreachable in
            // practice; treat any other transport error as terminal anyway
            Err(_) => HandleOutcome::Shutdown,
        },
        // an empty buffer carries no request — nothing to do
        Ok(None) => HandleOutcome::Continue,
        // the frame decoded structurally but its payload was invalid (bad
        // dates, unknown enum names); the buffer is in sync, so answer with a
        // recoverable violation and keep serving
        Err(FrameError::Decode(err)) => {
            let _ = sink.send(&protocol_violation(&format!("malformed request: {err}")));
            HandleOutcome::Continue
        }
        // framing itself is broken — unrecoverable by design
        Err(err) => {
            let _ = sink.send(&child.fatal_event(&format!("malformed request frame: {err}")));
            HandleOutcome::Shutdown
        }
    }
}

/// The current sandbox budget visible to an external host.
///
/// Hosts use the memory fields to arm their allocator and `max_suspensions` to
/// restore their accounting.
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionBudget {
    /// `max_memory` in bytes; `None` when unlimited, or when no session exists.
    pub max_memory: Option<usize>,
    /// Whether the session type checks each fed snippet.
    pub type_check: bool,
    /// Maximum suspensions the host may service; enforced outside the child.
    /// `None` only when no session exists.
    pub max_suspensions: Option<usize>,
    /// Host-enforced sleep budget; `None` when unlimited or no session exists.
    pub max_total_sleep: Option<Duration>,
}

/// REPL session state of the child.
enum SessionState<E: Engine> {
    /// No repl materialized yet. `Some` once `Configure` has stored the config
    /// (the repl is built lazily on the first `Feed` / `Dump`); `None` on a
    /// freshly spawned or just-`Reset` worker, before `Configure`. `Load` is
    /// valid only from here — it cannot clobber a started session.
    Configured(Option<Box<pb::Configure>>),
    /// Session ready for the next `Feed`.
    Ready(Box<E::Repl>),
    /// Mid-feed, waiting for a resume request. Never holds a completed feed —
    /// completion ends the turn immediately.
    Suspended(Box<E::Progress>),
    /// A feed parked until the host answers the engine's pre-feed ask
    /// (Python: module stubs before type checking).
    AwaitingPreFeed(Box<PendingFeed<E>>),
}

/// A feed held until the parent answers the engine's pre-feed ask.
struct PendingFeed<E: Engine> {
    repl: Box<E::Repl>,
    code: String,
    inputs: NamedValues,
    /// The `call_id` of the ask, which the `ResumeCall` must match.
    call_id: u32,
}

/// All state of one protocol child: the current REPL session plus the
/// per-session metadata (script name, print pacing, OS policy) that lives
/// outside the engine.
///
/// The child performs no filesystem I/O: mounts are host configuration the
/// parent handles entirely by servicing filesystem `OsCall` events itself, so
/// no mount state (or host path) ever reaches the child.
///
/// Drive it by reading framed [`pb::ParentRequest`]s from the host transport
/// and passing each to [`Self::handle`] along with an [`EventSink`]; the child
/// streams `Print` events and one turn-ending event per request.
pub struct Child<E: Engine> {
    state: SessionState<E>,
    /// Script name of the current session (used for error and type-check
    /// diagnostics).
    script_name: String,
    /// How long [`ProtoPrint`] may hold buffered output, from the session's
    /// `Configure`. `Duration::ZERO` means line buffering (see the field's
    /// documentation in the schema).
    print_flush_interval: Duration,
    /// The session's `OsPolicy` from `Configure`, applied when creating the REPL.
    os_policy: OsPolicy,
    /// The language runtime.
    engine: E,
}

impl<E: Engine> Default for Child<E> {
    fn default() -> Self {
        Self {
            state: SessionState::Configured(None),
            script_name: String::new(),
            print_flush_interval: DEFAULT_PRINT_FLUSH_INTERVAL,
            os_policy: OsPolicy::default(),
            engine: E::default(),
        }
    }
}

impl<E: Engine> Child<E> {
    /// Handles one request: streams any `Print` events and emits exactly one
    /// turn-ending event through `sink`, then reports what the host loop should
    /// do next. `Err` means the sink is broken (for stdout, the parent is
    /// gone).
    pub fn handle(
        &mut self,
        request: pb::ParentRequest,
        sink: &mut dyn EventSink,
    ) -> Result<HandleOutcome, FrameError> {
        let Some(kind) = request.kind else {
            sink.send(&protocol_violation("request has no kind"))?;
            return Ok(HandleOutcome::Continue);
        };

        let mut event = match kind {
            pb::parent_request::Kind::Configure(configure) => {
                // An unsupported protocol version is fatal: the parent may frame
                // or interpret later messages differently, so serving it risks a
                // silent desync. Emit the fatal last gasp and stop the child.
                if let Err(refusal) = check_protocol_version(configure.protocol_version) {
                    sink.send(&self.fatal_event(&refusal))?;
                    return Ok(HandleOutcome::Fatal);
                }
                self.handle_configure(configure)
            }
            pb::parent_request::Kind::Feed(feed) => self.handle_repl_feed(feed, sink),
            // The Monty sandbox has no host interpreter to install packages for;
            // dependency installation is only supported by the CPython worker.
            // Answer with a session-preserving error rather than a hard failure.
            pb::parent_request::Kind::InstallDependencies(_) => error_event(
                ExcType::RuntimeError,
                "dependency installation is only supported by the CPython worker",
            ),
            pb::parent_request::Kind::ResumeCall(resume) => self.handle_resume_call(resume, sink),
            pb::parent_request::Kind::ResumeNameLookup(resume) => self.handle_resume_name_lookup(resume, sink),
            pb::parent_request::Kind::ResumeFutures(resume) => self.handle_resume_futures(resume, sink),
            pb::parent_request::Kind::AbortFeed(abort) => self.handle_abort_feed(abort, sink),
            pb::parent_request::Kind::Dump(_) => self.handle_dump(),
            pb::parent_request::Kind::Load(load) => self.handle_load(&load),
            pb::parent_request::Kind::Reset(_) => match self.reset() {
                Ok(()) => ok_event(),
                // A failed scrub leaves the finished session's files in the
                // type checker, so this worker must never serve another one:
                // the next session could resolve the previous session's
                // modules. Die with an explanation the parent can log rather
                // than carry on — or panic, which it would only see as a crash.
                Err(err) => {
                    sink.send(&self.fatal_event(&format!("type-check cleanup failed: {err}")))?;
                    return Ok(HandleOutcome::Fatal);
                }
            },
            pb::parent_request::Kind::Shutdown(_) => {
                sink.send(&ok_event())?;
                return Ok(HandleOutcome::Shutdown);
            }
        };
        self.stamp_session_budget(&mut event);
        let sent = sink.send(&event);
        // a suspension announcement was *lent* the payload it announces, so
        // take it back before anything can observe the stored suspension
        // without it — on the failed-send path too, since the session may yet
        // be dumped or answered
        if let Err(err) = self.reclaim_suspension_payload(&mut event) {
            sink.send(&self.fatal_event(&format!("suspension payload could not be restored: {err}")))?;
            return Ok(HandleOutcome::Fatal);
        }
        if let Err(err) = sent {
            self.recover_send_error(&event, err, sink)?;
        }
        Ok(HandleOutcome::Continue)
    }

    /// Moves a suspension announcement's payload back into the suspension it
    /// announces, undoing the loan taken by [`suspension_event`]. A no-op for
    /// every other event.
    ///
    /// The gap this closes is short by construction: the payload is lent as the
    /// event is built and reclaimed as soon as [`Self::handle`] has sent it,
    /// with only timing stamps and print draining in between.
    ///
    /// `Err` means the wire arms and the engine's payloads have drifted apart —
    /// the conversion back is total for a payload this child just produced, so
    /// a failure would leave a suspension the parent's answer can no longer be
    /// applied to. The caller makes that fatal rather than serving on.
    fn reclaim_suspension_payload(&mut self, event: &mut pb::ChildEvent) -> Result<(), ProtoConvertError> {
        let SessionState::Suspended(progress) = &mut self.state else {
            return Ok(());
        };
        match (E::suspension(progress), &mut event.kind) {
            (Suspension::FunctionCall { args, .. }, Some(pb::child_event::Kind::FunctionCall(announced))) => {
                *args = mem::take(announced).into_call_args()?;
            }
            (Suspension::OsCall { call, .. }, Some(pb::child_event::Kind::OsCall(announced)))
                if announced.call.is_some() =>
            {
                *call = os_call_from_proto(mem::take(announced))?.1;
            }
            // any other pairing is an event that borrowed nothing
            _ => {}
        }
        Ok(())
    }

    /// What the session the child is *currently* holding would run under.
    ///
    /// A host that bounds the worker process from outside the interpreter (the
    /// subprocess shell caps its own allocator) sizes that bound from this,
    /// after every request: the budget changes when a session is configured,
    /// restored from a dump with limits capped by `Configure`, or ended by `Reset`.
    #[must_use]
    pub fn session_budget(&self) -> SessionBudget {
        match &self.state {
            SessionState::Configured(Some(config)) => SessionBudget {
                max_memory: config
                    .limits
                    .as_ref()
                    .and_then(|limits| limits.max_memory_bytes)
                    .map(|v| usize::try_from(v).unwrap_or(usize::MAX)),
                type_check: config.type_check,
                // the wire default applies before the repl exists too
                max_suspensions: Some(ResourceLimits::from(config.limits.unwrap_or_default()).max_suspensions),
                max_total_sleep: config
                    .limits
                    .as_ref()
                    .and_then(|limits| limits.max_total_sleep_micros)
                    .map(Duration::from_micros),
            },
            SessionState::Configured(None) => SessionBudget::default(),
            SessionState::Ready(repl) => self.tracker_budget(E::repl_tracker(repl)),
            SessionState::Suspended(progress) => self.tracker_budget(E::progress_tracker(progress)),
            SessionState::AwaitingPreFeed(pending) => self.tracker_budget(E::repl_tracker(&pending.repl)),
        }
    }

    /// The budget of a materialized session, whose limits live in its tracker.
    fn tracker_budget(&self, tracker: &ResourceTracker) -> SessionBudget {
        SessionBudget {
            max_memory: tracker.max_memory(),
            type_check: self.engine.type_checks(),
            max_suspensions: Some(tracker.max_suspensions()),
            max_total_sleep: tracker.max_total_sleep(),
        }
    }

    /// Builds a timing-stamped `FatalError` event for an unrecoverable
    /// condition the host detected (frame desync, oversize request). The host
    /// sends it and exits right after; it is the child's parseable last gasp.
    #[must_use]
    pub fn fatal_event(&self, message: &str) -> pb::ChildEvent {
        let mut event = fatal_error_event(message);
        // fatal paths bypass `handle`, so stamp timing here to keep the
        // "every turn-ending event carries timing" contract intact
        self.stamp_session_budget(&mut event);
        event
    }

    /// Recovers from a failure to write a turn-ending event.
    ///
    /// [`write_frame`] rejects an oversize frame *before* writing any bytes, so
    /// the stream stays synced and an oversize event (a large `Complete`, or a
    /// `DumpResult` while suspended) can be answered with a clean,
    /// session-preserving error. An oversize *suspension announcement* is
    /// unrecoverable — the worker is suspended but the parent never learned the
    /// resume point — so it propagates to the host loop's fatal handling, as
    /// does any genuine I/O break.
    fn recover_send_error(
        &mut self,
        failed: &pb::ChildEvent,
        err: FrameError,
        sink: &mut dyn EventSink,
    ) -> Result<(), FrameError> {
        let announces_suspension = matches!(
            failed.kind,
            Some(
                pb::child_event::Kind::FunctionCall(_)
                    | pb::child_event::Kind::OsCall(_)
                    | pb::child_event::Kind::NameLookup(_)
                    | pb::child_event::Kind::ResolveFutures(_)
            )
        );
        match err {
            FrameError::FrameTooLarge { len, max } if !announces_suspension => {
                let mut event = error_event(
                    ExcType::RuntimeError,
                    &format!("result frame of {len} bytes exceeds the maximum of {max} bytes"),
                );
                self.stamp_session_budget(&mut event);
                sink.send(&event)
            }
            other => Err(other),
        }
    }

    /// Stamps session timing and parent-enforced limits onto an event.
    ///
    /// Reported timing drives the parent's backstop. Fields are absent without
    /// a session.
    fn stamp_session_budget(&self, event: &mut pb::ChildEvent) {
        let tracker = match &self.state {
            SessionState::Ready(repl) => E::repl_tracker(repl),
            SessionState::Suspended(progress) => E::progress_tracker(progress),
            SessionState::AwaitingPreFeed(pending) => E::repl_tracker(&pending.repl),
            // no repl materialized yet → no tracker to report
            SessionState::Configured(_) => return,
        };
        stamp_budget(event, tracker);
    }

    /// Stores the session config; the repl is built lazily by [`ensure_repl`]
    /// on the first feed/dump (or restored by `Load` instead). Valid only on a
    /// not-yet-configured worker.
    fn handle_configure(&mut self, configure: pb::Configure) -> pb::ChildEvent {
        if matches!(self.state, SessionState::Configured(None)) {
            // Applied on arrival rather than in `ensure_repl`, which a `Load`
            // never reaches: a dump restores the repl directly, and print
            // pacing is a delivery setting the dump does not carry.
            // Absent means an older parent, or one with no opinion; both get
            // the default rather than the line buffering that predates it.
            self.print_flush_interval = configure
                .print_flush_interval_ms
                .map_or(DEFAULT_PRINT_FLUSH_INTERVAL, |ms| Duration::from_millis(u64::from(ms)));
            // Reject invalid settings on the Configure turn.
            self.os_policy = match configure.os_policy.clone().map(OsPolicy::try_from) {
                None => OsPolicy::default(),
                Some(Ok(os_policy)) => os_policy,
                Some(Err(err)) => return protocol_violation(&format!("invalid os_policy: {err}")),
            };
            if let Err(event) = self.engine.configure(&configure) {
                return *event;
            }
            self.state = SessionState::Configured(Some(Box::new(configure)));
            ok_event()
        } else {
            protocol_violation("Configure while a session already exists")
        }
    }

    /// Materializes the repl from the stored config the first time the session
    /// runs (feed/dump), applying the config's script name and limits. A no-op
    /// once the repl exists; errors only if the worker was never configured
    /// (which the pool's `Configure`-first checkout prevents in normal
    /// operation).
    fn ensure_repl(&mut self) -> Result<(), Box<pb::ChildEvent>> {
        let config = match &mut self.state {
            SessionState::Configured(config) => config.take(),
            // already materialized (or mid-feed) — nothing to do here
            SessionState::Ready(_) | SessionState::Suspended(_) | SessionState::AwaitingPreFeed(_) => return Ok(()),
        };
        let Some(config) = config else {
            return Err(Box::new(protocol_violation("session has not been configured")));
        };
        // Destructured exhaustively on purpose: a new `Configure` field must
        // fail to compile here until the child decides what to do with it.
        let pb::Configure {
            script_name,
            limits,
            // the engine's, read by `Engine::configure` when the `Configure` arrived
            type_check: _,
            type_check_stubs: _,
            assert_message_annotations: _,
            type_check_format: _,
            type_check_color: _,
            // range-checked when `Configure` arrived
            protocol_version: _,
            // informational only — never checked
            monty_version: _,
            // applied when the `Configure` arrived, so a `Load` honors it too
            print_flush_interval_ms: _,
            // validated and stored when the `Configure` arrived
            os_policy: _,
            // a relay's concern; the child never stores sessions
            persistence: _,
            // a relay's concern; the child has no profiles
            profile: _,
        } = *config;
        let limits = ResourceLimits::from(limits.unwrap_or_default());
        self.script_name = script_name;
        let repl = self
            .engine
            .create_repl(&self.script_name, limits, self.os_policy.clone());
        self.state = SessionState::Ready(Box::new(repl));
        Ok(())
    }

    /// Checks the snippet and lets the engine prepare it (Python requests
    /// missing host stubs and type checks) before starting the feed.
    fn handle_repl_feed(&mut self, feed: pb::Feed, sink: &mut dyn EventSink) -> pb::ChildEvent {
        if let Err(event) = self.ensure_repl() {
            return *event;
        }
        if !matches!(self.state, SessionState::Ready(_)) {
            return protocol_violation("Feed without a session ready for input");
        }
        let inputs = match named_values_from_proto(feed.inputs, feed.values) {
            Ok(inputs) => inputs,
            Err(err) => return protocol_violation(&format!("invalid inputs: {err}")),
        };
        let SessionState::Ready(mut repl) = mem::replace(&mut self.state, SessionState::Configured(None)) else {
            unreachable!("checked Ready above");
        };
        // TODO(feed-language): once `Feed` names its language on the wire, reject
        // one outside `E::LANGUAGES` here and pass it to the engine; until then
        // every feed runs in the engine's default, `E::LANGUAGES[0]`.
        let code = feed.code;
        // Check the snippet before the engine's pre-feed step or the compiler parses it.
        if let Err(error) = E::check_source(&repl, &code) {
            self.state = SessionState::Ready(repl);
            return error_event_from(&error);
        }
        // The working directory persists in the REPL (including `os.chdir`); the parent
        // sends one only to switch it, and the switch holds whatever the feed's outcome.
        if !feed.cwd.is_empty() {
            E::set_cwd(&mut repl, &feed.cwd);
        }
        let type_check = !feed.skip_type_check && self.engine.type_checks();
        let pre_feed = self.engine.pre_feed(&repl, &self.script_name, &code, type_check);
        self.continue_feed(pre_feed, repl, code, inputs, sink)
    }

    /// Acts on the engine's pre-feed decision: run the snippet, park it until
    /// the host answers, or reject it.
    fn continue_feed(
        &mut self,
        pre_feed: PreFeed,
        repl: Box<E::Repl>,
        code: String,
        inputs: NamedValues,
        sink: &mut dyn EventSink,
    ) -> pb::ChildEvent {
        match pre_feed {
            PreFeed::Run => self.start_feed(*repl, &code, inputs, sink),
            PreFeed::Ask(event) => {
                if let Some(call_id) = ask_call_id(&event) {
                    self.state = SessionState::AwaitingPreFeed(Box::new(PendingFeed {
                        repl,
                        code,
                        inputs,
                        call_id,
                    }));
                    event
                } else {
                    self.state = SessionState::Ready(repl);
                    self.engine.feed_settled(false);
                    protocol_violation("the engine's pre-feed ask is not an OS call")
                }
            }
            PreFeed::Reject(event) => {
                self.state = SessionState::Ready(repl);
                self.engine.feed_settled(false);
                event
            }
            PreFeed::Invalid(event) => {
                // only meaningful as an answer to a parked feed; a fresh feed has no answer to reject
                self.state = SessionState::Ready(repl);
                self.engine.feed_settled(false);
                event
            }
        }
    }

    /// Injects inputs and executes to the turn-ending event.
    fn start_feed(
        &mut self,
        repl: E::Repl,
        code: &str,
        inputs: NamedValues,
        sink: &mut dyn EventSink,
    ) -> pb::ChildEvent {
        let mut print = ProtoPrint::new(sink, self.print_flush_interval);
        let result = self
            .engine
            .feed_start(repl, code, inputs, PrintWriter::Callback(&mut print));
        let event = self.drive(result);
        print.drain();
        event
    }

    /// Hands the parent's answer to a parked feed's ask back to the engine.
    fn handle_resume_pre_feed(&mut self, resume: pb::ResumeCall, sink: &mut dyn EventSink) -> pb::ChildEvent {
        let SessionState::AwaitingPreFeed(pending) = &self.state else {
            unreachable!("checked by the caller");
        };
        if resume.call_id != pending.call_id {
            return protocol_violation(&format!(
                "ResumeCall call_id {} does not match {}",
                resume.call_id, pending.call_id
            ));
        }
        let Some(wire_result) = resume.result else {
            return protocol_violation("ResumeCall has no result");
        };
        let answer = if matches!(wire_result.kind, Some(pb::ext_function_result::Kind::NotHandled(_))) {
            None
        } else {
            match ext_result_from_proto(wire_result, resume.values) {
                Ok(result) => Some(result),
                Err(err) => return protocol_violation(&format!("invalid result: {err}")),
            }
        };
        let SessionState::AwaitingPreFeed(pending) = mem::replace(&mut self.state, SessionState::Configured(None))
        else {
            unreachable!("checked above");
        };
        let pre_feed = self
            .engine
            .resume_pre_feed(&pending.repl, &self.script_name, &pending.code, answer);
        let PendingFeed {
            repl,
            code,
            inputs,
            call_id,
        } = *pending;
        if let PreFeed::Invalid(event) = pre_feed {
            // the answer was rejected; the feed keeps waiting for a valid one
            self.state = SessionState::AwaitingPreFeed(Box::new(PendingFeed {
                repl,
                code,
                inputs,
                call_id,
            }));
            event
        } else {
            self.continue_feed(pre_feed, repl, code, inputs, sink)
        }
    }

    /// Returns the exception and readies the session after a feed fails before execution.
    fn fail_pending_feed(&mut self, pending: PendingFeed<E>, exc: &MontyException) -> pb::ChildEvent {
        self.state = SessionState::Ready(pending.repl);
        self.engine.feed_settled(false);
        error_event_from(exc)
    }

    /// Answers a suspended external function or OS call with the parent's
    /// result, checking the `call_id` matches, then resumes execution.
    fn handle_resume_call(&mut self, resume: pb::ResumeCall, sink: &mut dyn EventSink) -> pb::ChildEvent {
        if matches!(self.state, SessionState::AwaitingPreFeed(_)) {
            return self.handle_resume_pre_feed(resume, sink);
        }
        let SessionState::Suspended(progress) = &mut self.state else {
            return protocol_violation("ResumeCall without a suspended function/OS call");
        };
        let Some(wire_result) = resume.result else {
            return protocol_violation("ResumeCall has no result");
        };
        let not_handled = matches!(wire_result.kind, Some(pb::ext_function_result::Kind::NotHandled(_)));
        // NotHandled resolves against the suspended call itself — the child
        // owns the no-handler semantics (`OsFunctionCall::on_no_handler`), so
        // the parent never has to compute or echo the default exception.
        let no_handler = match E::suspension(progress) {
            Suspension::FunctionCall { call_id, .. } if call_id == resume.call_id => {
                if not_handled {
                    return protocol_violation("NotHandled is only valid answering a suspended OS call");
                }
                None
            }
            Suspension::OsCall { call, call_id, .. } if call_id == resume.call_id => {
                not_handled.then(|| call.on_no_handler())
            }
            Suspension::FunctionCall { call_id, .. } | Suspension::OsCall { call_id, .. } => {
                return protocol_violation(&format!(
                    "ResumeCall call_id {} does not match {call_id}",
                    resume.call_id
                ));
            }
            Suspension::NameLookup { .. } | Suspension::ResolveFutures { .. } => {
                return protocol_violation("ResumeCall without a suspended function/OS call");
            }
        };
        let result = match no_handler {
            Some(exc) => ExtFunctionResult::Error(exc),
            None => match ext_result_from_proto(wire_result, resume.values) {
                Ok(result) => result,
                Err(err) => return protocol_violation(&format!("invalid result: {err}")),
            },
        };
        let SessionState::Suspended(progress) = mem::replace(&mut self.state, SessionState::Configured(None)) else {
            unreachable!("checked above");
        };
        let mut print = ProtoPrint::new(sink, self.print_flush_interval);
        let outcome = self
            .engine
            .resume_call(*progress, result, PrintWriter::Callback(&mut print));
        let event = self.drive(outcome);
        print.drain();
        event
    }

    /// Answers a suspended name lookup with the value (or absence) the parent
    /// resolved, then resumes execution.
    fn handle_resume_name_lookup(&mut self, resume: pb::ResumeNameLookup, sink: &mut dyn EventSink) -> pb::ChildEvent {
        let SessionState::Suspended(progress) = &mut self.state else {
            return protocol_violation("ResumeNameLookup without a suspended name lookup");
        };
        if !matches!(E::suspension(progress), Suspension::NameLookup { .. }) {
            return protocol_violation("ResumeNameLookup without a suspended name lookup");
        }
        let result = match resume.try_into() {
            Ok(result) => result,
            Err(err) => return protocol_violation(&format!("invalid result: {err}")),
        };
        let SessionState::Suspended(progress) = mem::replace(&mut self.state, SessionState::Configured(None)) else {
            unreachable!("checked above");
        };
        let mut print = ProtoPrint::new(sink, self.print_flush_interval);
        let outcome = self
            .engine
            .resume_name_lookup(*progress, result, PrintWriter::Callback(&mut print));
        let event = self.drive(outcome);
        print.drain();
        event
    }

    /// Raises the parent's exception uncatchably at any pending suspension.
    /// The `Error` reply returns the session to `Ready`.
    fn handle_abort_feed(&mut self, abort: pb::AbortFeed, sink: &mut dyn EventSink) -> pb::ChildEvent {
        if !matches!(
            self.state,
            SessionState::Suspended(_) | SessionState::AwaitingPreFeed(_)
        ) {
            return protocol_violation("AbortFeed without a suspended feed");
        }
        let Some(exception) = abort.exception else {
            return protocol_violation("AbortFeed has no exception");
        };
        let exc = match MontyException::try_from(exception) {
            Ok(exc) => exc,
            Err(err) => return protocol_violation(&format!("invalid exception: {err}")),
        };
        let progress = match mem::replace(&mut self.state, SessionState::Configured(None)) {
            SessionState::Suspended(progress) => progress,
            // a feed held for its ask never started, so there is nothing to unwind
            SessionState::AwaitingPreFeed(pending) => return self.fail_pending_feed(*pending, &exc),
            _ => unreachable!("checked above"),
        };
        let mut print = ProtoPrint::new(sink, self.print_flush_interval);
        let outcome = self.engine.abort(*progress, exc, PrintWriter::Callback(&mut print));
        let event = self.drive(outcome);
        print.drain();
        event
    }

    /// Delivers settled futures to a `ResolveFutures` suspension, or one
    /// settled coroutine to the function call that allowed an eager reply.
    fn handle_resume_futures(&mut self, resume: pb::ResumeFutures, sink: &mut dyn EventSink) -> pb::ChildEvent {
        let results = match future_results_from_proto(resume.results, resume.values) {
            Ok(results) => results,
            Err(err) => return protocol_violation(&format!("invalid results: {err}")),
        };
        // Shaped against the borrowed state, so a rejected reply leaves the suspension intact.
        let SessionState::Suspended(progress) = &mut self.state else {
            return protocol_violation("ResumeFutures without suspended futures");
        };
        let reply = match E::suspension(progress) {
            Suspension::FunctionCall {
                call_id,
                allow_eager_await: true,
                ..
            }
            | Suspension::OsCall {
                call_id,
                allow_eager_await: true,
                ..
            } => match eager_result(results, call_id) {
                Ok(result) => FuturesReply::Eager(result),
                Err(message) => return protocol_violation(message),
            },
            Suspension::ResolveFutures { .. } => FuturesReply::Batch(results),
            Suspension::FunctionCall { .. } | Suspension::OsCall { .. } | Suspension::NameLookup { .. } => {
                return protocol_violation("ResumeFutures without suspended futures");
            }
        };
        let SessionState::Suspended(progress) = mem::replace(&mut self.state, SessionState::Configured(None)) else {
            unreachable!("checked above");
        };
        let mut print = ProtoPrint::new(sink, self.print_flush_interval);
        let outcome = match reply {
            FuturesReply::Eager(result) => {
                self.engine
                    .resume_eager(*progress, result, PrintWriter::Callback(&mut print))
            }
            FuturesReply::Batch(results) => {
                self.engine
                    .resume_futures(*progress, results, PrintWriter::Callback(&mut print))
            }
        };
        let event = self.drive(outcome);
        print.drain();
        event
    }

    /// Serializes the current session, and the metadata that lives outside it,
    /// into the engine's dump format. The session stays live — dumping is read-only.
    fn handle_dump(&mut self) -> pb::ChildEvent {
        // a never-fed session is materialized into an empty repl so it can be
        // dumped; a never-configured worker has nothing to dump
        if let Err(event) = self.ensure_repl() {
            return *event;
        }
        let session = match &self.state {
            SessionState::Ready(repl) => SessionRef::Idle(&**repl),
            SessionState::Suspended(progress) => SessionRef::Suspended(&**progress),
            // the feed is not yet part of the session, so the dump could not carry it
            SessionState::AwaitingPreFeed(_) => {
                return error_event(
                    ExcType::RuntimeError,
                    "cannot dump while a feed awaits the host before it starts",
                );
            }
            SessionState::Configured(_) => unreachable!("ensure_repl materialized the repl or errored"),
        };
        match self.engine.dump(&self.script_name, session) {
            Ok(state) => event(pb::child_event::Kind::DumpResult(pb::DumpResult {
                state: state.into(),
            })),
            Err(err) => protocol_violation(&format!("dump failed: {err}")),
        }
    }

    /// Restores a dump produced by [`Self::handle_dump`] into this child. A
    /// restored suspension re-emits its suspension event so the parent learns
    /// the resume point.
    ///
    /// `Load` is valid only when no repl has been materialized yet — a freshly
    /// checked-out (`Configure`d, unfed) worker — so it initializes the session
    /// instead of feeding. Once a feed has run (or a prior `Load` restored a
    /// session), the repl exists and `Load` is rejected rather than silently
    /// discarding it.
    fn handle_load(&mut self, load: &pb::Load) -> pb::ChildEvent {
        let limits = match &self.state {
            SessionState::Configured(config) => {
                ResourceLimits::from(config.as_ref().and_then(|config| config.limits).unwrap_or_default())
            }
            _ => return protocol_violation("Load requires a session that has not started (a feed has already run)"),
        };
        let Loaded {
            script_name,
            session,
            restored,
        } = match self.engine.load(&load.state, &limits) {
            Ok(loaded) => loaded,
            Err(event) => return *event,
        };
        // In-process Rust producers can dump suspensions exceeding the wire size limit;
        // check transport compatibility even though snapshot integrity is the host's responsibility.
        let mut event = match session {
            LoadedSession::Idle(repl) => {
                self.state = SessionState::Ready(Box::new(repl));
                ok_event()
            }
            LoadedSession::Complete { repl, value } => {
                self.state = SessionState::Ready(Box::new(repl));
                complete_event(value)
            }
            LoadedSession::Suspended(mut progress) => {
                let mut event = suspension_event(E::suspension(&mut progress));
                // size-checked with the stamps `handle` sends it with
                stamp_budget(&mut event, E::progress_tracker(&progress));
                if let Some(message) = oversize_suspension_error_message(&event) {
                    protocol_violation(&message)
                } else {
                    self.state = SessionState::Suspended(Box::new(progress));
                    event
                }
            }
        };
        // adopt the restored metadata only once the payload actually loaded
        // (state is now Ready/Suspended) — a failed load leaves the child in
        // its prior un-started state, re-loadable. Surface the adopted script
        // name so the parent can report it without parsing the opaque dump.
        if matches!(self.state, SessionState::Ready(_) | SessionState::Suspended(_)) {
            self.script_name = script_name;
            self.engine.adopt(restored);
            event.restored_script_name = Some(self.script_name.clone());
        }
        event
    }

    /// Runs until a turn-ending event. OS calls not answered by `OsPolicy`
    /// go to the parent, including all filesystem I/O.
    fn drive(&mut self, step: Step<E>) -> pb::ChildEvent {
        match step {
            Ok(Outcome::Complete { repl, value }) => {
                self.state = SessionState::Ready(Box::new(repl));
                self.engine.feed_settled(true);
                complete_event(value)
            }
            Ok(Outcome::Suspended(mut progress)) => {
                let mut event = suspension_event(E::suspension(&mut progress));
                // stamped before the size check, so the frame measured is
                // the frame `handle` sends
                stamp_budget(&mut event, E::progress_tracker(&progress));
                if let Some(message) = oversize_suspension_error_message(&event) {
                    self.abort_feed_with_runtime_error(E::into_repl(progress), &message)
                } else {
                    self.state = SessionState::Suspended(Box::new(progress));
                    event
                }
            }
            Err(failed) => {
                // sandbox-level failure: the session always survives
                let Failed { repl, error } = *failed;
                self.state = SessionState::Ready(Box::new(repl));
                self.engine.feed_settled(false);
                error_event_from(&error)
            }
        }
    }

    /// Ends the current feed with a runtime error while keeping the REPL usable.
    fn abort_feed_with_runtime_error(&mut self, repl: E::Repl, message: &str) -> pb::ChildEvent {
        self.state = SessionState::Ready(Box::new(repl));
        self.engine.feed_settled(false);
        error_event(ExcType::RuntimeError, message)
    }

    /// Drops all session state, returning to the unconfigured state ready for
    /// the next `Configure` (or `Load`).
    ///
    /// `Err` means the engine still holds this session's state (Python's type
    /// checker and its files), which is terminal for the worker — see the
    /// `Reset` arm in [`Self::handle`].
    fn reset(&mut self) -> Result<(), String> {
        self.state = SessionState::Configured(None);
        self.script_name = String::new();
        self.print_flush_interval = DEFAULT_PRINT_FLUSH_INTERVAL;
        self.os_policy = OsPolicy::default();
        self.engine.reset()
    }
}

/// Wraps an event kind into a `ChildEvent` with zeroed timing fields;
/// [`Child::handle`] (and [`Child::fatal_event`]) stamps the timing fields onto
/// every turn-ending event just before it is sent.
pub(crate) fn event(kind: pb::child_event::Kind) -> pb::ChildEvent {
    pb::ChildEvent {
        kind: Some(kind),
        ..Default::default()
    }
}

/// Builds the turn-ending event for a recoverable protocol violation (wrong
/// state, bad call id, invalid payload). The child's state is unchanged.
///
/// Public so a host transport can answer a frame that decoded but is not a
/// valid request (e.g. a malformed parent message) without reaching into the
/// event-kind types.
#[must_use]
pub fn protocol_violation(message: &str) -> pb::ChildEvent {
    event(pb::child_event::Kind::Error(pb::Error {
        exception: Some(pb::RaisedException {
            exc_type: ExcType::RuntimeError.to_string(),
            message: Some(format!("protocol violation: {message}")),
            traceback: BudgetVec::new(),
            data: None,
        }),
    }))
}

/// Builds an *unstamped* `FatalError` event.
///
/// Public for hosts that cannot stamp timing because no [`Child`] is in scope —
/// notably a panic hook firing on a thread that no longer owns the child. When
/// a child is available, prefer [`Child::fatal_event`], which stamps timing.
#[must_use]
pub fn fatal_error_event(message: &str) -> pb::ChildEvent {
    event(pb::child_event::Kind::FatalError(pb::FatalError {
        message: message.to_owned(),
    }))
}

fn ok_event() -> pb::ChildEvent {
    event(pb::child_event::Kind::Ok(pb::Ok {}))
}

/// Builds a turn-ending `Error` event from an exception type and message.
fn error_event(exc_type: ExcType, message: &str) -> pb::ChildEvent {
    event(pb::child_event::Kind::Error(pb::Error {
        exception: Some(pb::RaisedException {
            exc_type: exc_type.to_string(),
            message: Some(message.to_owned()),
            traceback: BudgetVec::new(),
            data: None,
        }),
    }))
}

/// Preserves the exception's traceback in the error event.
pub(crate) fn error_event_from(exc: &MontyException) -> pb::ChildEvent {
    event(pb::child_event::Kind::Error(pb::Error {
        exception: Some(exc.into()),
    }))
}

/// The `call_id` of an engine's pre-feed ask, which must be an `OsCall` event.
fn ask_call_id(event: &pb::ChildEvent) -> Option<u32> {
    match &event.kind {
        Some(pb::child_event::Kind::OsCall(call)) => Some(call.call_id),
        _ => None,
    }
}

/// Stamps `tracker`'s timing and parent-enforced limits onto an event. Called
/// again by [`Child::handle`] just before sending, so a suspension announcement
/// is size-checked with the stamps it will carry.
fn stamp_budget(event: &mut pb::ChildEvent, tracker: &ResourceTracker) {
    event.total_execution_micros = u64::try_from(tracker.elapsed().as_micros()).unwrap_or(u64::MAX);
    event.feed_execution_micros = u64::try_from(tracker.feed_elapsed().as_micros()).unwrap_or(u64::MAX);
    event.max_feed_duration_micros = micros_field(tracker.max_feed_duration());
    event.max_turn_duration_micros = micros_field(tracker.max_turn_duration());
    event.max_total_sleep_micros = micros_field(tracker.max_total_sleep());
    event.max_suspensions = Some(tracker.max_suspensions() as u64);
}

/// Describes a suspension announcement that would exceed the wire frame limit.
///
/// The child turns this into a host-visible error before entering the
/// suspension, because the parent cannot resume a call it never received.
/// The event must already carry its session stamps (see [`stamp_budget`]).
fn oversize_suspension_error_message(event: &pb::ChildEvent) -> Option<String> {
    exceeds_max_frame_len(event)
        .map(|len| format!("argument frame of {len} bytes exceeds the maximum of {MAX_FRAME_LEN} bytes"))
}

/// Builds the suspension event for what a feed is waiting on, **moving** a
/// function call's arguments or an OS call's payload into it.
///
/// The suspension keeps its payload — a `Dump` of the suspended state (and its
/// replay on `Load`) needs it — so it comes back via
/// [`Child::reclaim_suspension_payload`] once the event has been sent. It is
/// lent rather than copied because it is the largest thing a suspension
/// carries and is already live twice over here (the interpreter's own values,
/// plus this converted copy): a third copy for the announcement, on top of the
/// encode buffer, is what used to push a large host-call argument past the
/// session's memory limit. `GetEnviron` is the placeholder left behind in an
/// OS call: a unit variant, so the swap allocates nothing.
fn suspension_event(suspension: Suspension<'_>) -> pb::ChildEvent {
    match suspension {
        Suspension::FunctionCall {
            function_name,
            args,
            call_id,
            object_id,
            allow_eager_await,
            position,
        } => event(pb::child_event::Kind::FunctionCall(WireFunctionCall::new(
            function_name.to_owned(),
            mem::take(args),
            call_id,
            object_id,
            allow_eager_await,
            position.clone(),
        ))),
        Suspension::OsCall {
            call,
            call_id,
            allow_eager_await,
            position,
        } => {
            let function_call = mem::replace(call, monty_types::OsFunctionCall::GetEnviron);
            event(pb::child_event::Kind::OsCall(os_call_to_proto(
                call_id,
                function_call,
                allow_eager_await,
                position,
            )))
        }
        Suspension::NameLookup {
            name,
            object_id,
            position,
        } => event(pb::child_event::Kind::NameLookup(pb::NameLookup {
            name: name.to_owned(),
            object_id: object_id.as_ref().map(uuid_to_pb),
            position: Some(position.into()),
        })),
        Suspension::ResolveFutures {
            pending_call_ids,
            position,
        } => event(pb::child_event::Kind::ResolveFutures(pb::ResolveFutures {
            pending_call_ids: pending_call_ids.to_vec().into(),
            position: Some(position.into()),
        })),
    }
}

fn complete_event(value: MontyObject) -> pb::ChildEvent {
    event(pb::child_event::Kind::Complete(value.into()))
}

/// A validated `ResumeFutures` body, shaped for the suspension it answers.
enum FuturesReply {
    /// One settled coroutine for a call with `allow_eager_await`.
    Eager(Result<MontyObject, MontyException>),
    /// Results for a `ResolveFutures` suspension.
    Batch(Vec<(u32, ExtFunctionResult)>),
}

/// Checks an eager reply is exactly one settled result for `call_id`; the
/// error is the protocol-violation message.
fn eager_result(
    results: Vec<(u32, ExtFunctionResult)>,
    call_id: u32,
) -> Result<Result<MontyObject, MontyException>, &'static str> {
    match <[_; 1]>::try_from(results) {
        Ok([(id, ExtFunctionResult::Return(value))]) if id == call_id => Ok(Ok(value)),
        Ok([(id, ExtFunctionResult::Error(exc))]) if id == call_id => Ok(Err(exc)),
        Ok([(id, _)]) if id == call_id => Err("eager coroutine must resolve to a value or exception"),
        _ => Err("eager ResumeFutures must contain exactly the suspended call id"),
    }
}
