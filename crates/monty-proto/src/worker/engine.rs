//! The seam between the protocol child and a language runtime.
//!
//! [`Child`](super::Child) owns everything the wire defines — request
//! dispatch, the session state machine, call-id validation, event framing,
//! budget stamping, print pacing — and asks an [`Engine`] for everything the
//! language defines: how a session is created, fed, suspended, resumed, dumped
//! and restored. The Python interpreter implements it in `python.rs`; the
//! JavaScript engine implements it in its own runtime crate.

use monty_types::{
    CallArgs, ExtFunctionResult, Language, MontyException, MontyObject, MontyUuid, NameLookupResult, NamedValues,
    OsFunctionCall, OsPolicy, PrintWriter, ResourceLimits, ResourceTracker, SourceRange,
};

use crate::pb;

/// A language runtime driven by the protocol child.
///
/// Every method that runs sandbox code takes the `PrintWriter` the child built
/// for the turn, so `print` output streams as `Print` events while the engine
/// runs. Methods that move a `Repl` or `Progress` give it back inside the
/// returned [`Step`], so the session survives whatever the engine does.
pub trait Engine: Default {
    /// An idle session, between feeds.
    type Repl;
    /// A feed suspended on the host; never a completed one.
    type Progress;
    /// Session metadata a dump carries beside the session itself (type-check
    /// state for Python), handed to [`adopt`](Self::adopt) once a `Load` succeeds.
    type Restored;

    /// Languages this engine compiles; the first is the runtime's default.
    const LANGUAGES: &'static [Language];

    /// Validates and records the engine's own `Configure` fields (type checking,
    /// assert annotations). Protocol-level fields — version, print pacing, OS
    /// policy, limits — are the child's. `Err` is the turn-ending event (boxed:
    /// events are large and the `Ok` path is the common one).
    fn configure(&mut self, configure: &pb::Configure) -> Result<(), Box<pb::ChildEvent>>;

    /// Whether the session type checks fed snippets, for the host's budget report.
    fn type_checks(&self) -> bool;

    /// Builds the idle session the first feed or dump materializes. `Err` is the
    /// turn-ending event (a limit too small for the realm); the child then forgets
    /// the configuration so the host can send another.
    fn create_repl(
        &mut self,
        script_name: &str,
        limits: ResourceLimits,
        os_policy: OsPolicy,
    ) -> Result<Self::Repl, Box<pb::ChildEvent>>;

    /// Rejects a snippet before anything parses it (source nesting, size).
    fn check_source(repl: &Self::Repl, code: &str) -> Result<(), MontyException>;

    /// Switches the session's working directory.
    fn set_cwd(repl: &mut Self::Repl, cwd: &str);

    /// What must happen before `code` executes: nothing, a host round-trip, or
    /// a rejection. Python asks the host for module stubs and type checks.
    fn pre_feed(&mut self, repl: &Self::Repl, script_name: &str, code: &str, type_check: bool) -> PreFeed;

    /// Continues [`pre_feed`](Self::pre_feed) with the host's answer to its ask;
    /// `None` when the host had no handler for it.
    fn resume_pre_feed(
        &mut self,
        repl: &Self::Repl,
        script_name: &str,
        code: &str,
        answer: Option<ExtFunctionResult>,
    ) -> PreFeed;

    /// Compiles and runs `code` until it completes, suspends or fails.
    fn feed_start(&mut self, repl: Self::Repl, code: &str, inputs: NamedValues, print: PrintWriter<'_>) -> Step<Self>;

    /// Called once a feed has ended: completed (`admitted`) or failed, aborted
    /// or rejected. Python commits or discards the snippet's type-check context.
    fn feed_settled(&mut self, admitted: bool);

    /// Answers a `FunctionCall` or `OsCall` suspension.
    fn resume_call(
        &mut self,
        progress: Self::Progress,
        result: ExtFunctionResult,
        print: PrintWriter<'_>,
    ) -> Step<Self>;

    /// Answers a call that allowed an eager await with its settled coroutine.
    fn resume_eager(
        &mut self,
        progress: Self::Progress,
        result: Result<MontyObject, MontyException>,
        print: PrintWriter<'_>,
    ) -> Step<Self>;

    /// Answers a `NameLookup` suspension.
    fn resume_name_lookup(
        &mut self,
        progress: Self::Progress,
        result: NameLookupResult,
        print: PrintWriter<'_>,
    ) -> Step<Self>;

    /// Delivers settled futures to a `ResolveFutures` suspension.
    fn resume_futures(
        &mut self,
        progress: Self::Progress,
        results: Vec<(u32, ExtFunctionResult)>,
        print: PrintWriter<'_>,
    ) -> Step<Self>;

    /// Raises `exc` uncatchably at the suspension point.
    fn abort(&mut self, progress: Self::Progress, exc: MontyException, print: PrintWriter<'_>) -> Step<Self>;

    /// What a suspended feed is waiting for, with mutable access to the
    /// payloads the announcement lends to the wire.
    fn suspension(progress: &mut Self::Progress) -> Suspension<'_>;

    /// Discards a suspended feed, keeping the session.
    fn into_repl(progress: Self::Progress) -> Self::Repl;

    /// The idle session's resource tracker.
    fn repl_tracker(repl: &Self::Repl) -> &ResourceTracker;

    /// The suspended feed's resource tracker.
    fn progress_tracker(progress: &Self::Progress) -> &ResourceTracker;

    /// Serializes the session and the metadata that lives outside it; the
    /// error is the message of the `dump failed: …` protocol violation.
    fn dump(&self, script_name: &str, session: SessionRef<'_, Self>) -> Result<Vec<u8>, String>;

    /// Restores a dump, tightening its limits to `limits`. Nothing is adopted
    /// by the engine yet: the child calls [`adopt`](Self::adopt) once the
    /// restored session is in place. `Err` is the turn-ending event.
    fn load(&self, state: &[u8], limits: &ResourceLimits) -> Result<Loaded<Self>, Box<pb::ChildEvent>>;

    /// Takes on the metadata of a session that just loaded.
    fn adopt(&mut self, restored: Self::Restored);

    /// Drops all session state. `Err` is terminal for the worker (the child
    /// emits it as a `FatalError`).
    fn reset(&mut self) -> Result<(), String>;
}

/// How a feed proceeds after the engine runs or resumes its pre-feed step.
pub enum PreFeed {
    /// Nothing stands in the way: execute the snippet.
    Run,
    /// Ask the host first: an `OsCall` suspension event whose `call_id` the
    /// child validates the `ResumeCall` against. The feed is parked meanwhile.
    Ask(pb::ChildEvent),
    /// The snippet does not run; the session returns to ready with this event
    /// (a `TypingError`, or the exception a stubs answer carried).
    Reject(pb::ChildEvent),
    /// The host's answer was malformed: a protocol violation, with the feed
    /// still parked for a valid answer.
    Invalid(pb::ChildEvent),
}

/// What a run or resume produced: the session carries on idle or suspended,
/// or the feed failed and the session survives.
pub type Step<E> = Result<Outcome<E>, Box<Failed<E>>>;

/// A feed that completed or suspended.
pub enum Outcome<E: Engine> {
    /// The snippet completed with this value; the session is ready again.
    Complete {
        /// The session, ready for the next feed.
        repl: E::Repl,
        /// The snippet's result.
        value: MontyObject,
    },
    /// The snippet is waiting on the host.
    Suspended(E::Progress),
}

/// A feed that failed with a sandbox exception; the session survives.
pub struct Failed<E: Engine> {
    /// The session, ready for the next feed.
    pub repl: E::Repl,
    /// The exception that ended the feed.
    pub error: MontyException,
}

/// What a suspended feed is waiting for.
///
/// The wire payloads (`args`, `call`) are borrowed mutably because the
/// announcement *lends* them to the event and reclaims them once it is sent;
/// they are the largest thing a suspension carries and a dump needs them.
pub enum Suspension<'a> {
    /// An external function call, or a host method call when `object_id` is set.
    FunctionCall {
        /// The function or method name.
        function_name: &'a str,
        /// Positional and keyword arguments, one arena.
        args: &'a mut CallArgs,
        /// The id the host answers with.
        call_id: u32,
        /// The receiver of a host method call.
        object_id: Option<MontyUuid>,
        /// Whether the host may answer with a settled coroutine.
        allow_eager_await: bool,
        /// Where the call expression is in the source.
        position: &'a SourceRange,
    },
    /// An OS operation for the host to service.
    OsCall {
        /// The typed call.
        call: &'a mut OsFunctionCall,
        /// The id the host answers with.
        call_id: u32,
        /// Whether the host may answer with a future.
        allow_eager_await: bool,
        /// Where the call expression is in the source.
        position: &'a SourceRange,
    },
    /// An undefined name, or a lazy attribute of a host object when `object_id` is set.
    NameLookup {
        /// The name being looked up.
        name: &'a str,
        /// The host object whose attribute is read.
        object_id: Option<MontyUuid>,
        /// Where the name is in the source.
        position: &'a SourceRange,
    },
    /// Every task is blocked on host futures.
    ResolveFutures {
        /// The futures the sandbox waits on.
        pending_call_ids: &'a [u32],
        /// Where the main task's blocked `await` is in the source.
        position: &'a SourceRange,
    },
}

/// A borrowed session to dump.
pub enum SessionRef<'a, E: Engine> {
    /// Between feeds.
    Idle(&'a E::Repl),
    /// Mid-feed, waiting on the host.
    Suspended(&'a E::Progress),
}

/// A session restored from a dump.
pub struct Loaded<E: Engine> {
    /// The script name the dump was taken under.
    pub script_name: String,
    /// The restored session.
    pub session: LoadedSession<E>,
    /// Metadata for [`Engine::adopt`] once the session is in place.
    pub restored: E::Restored,
}

/// The state a dump restores to.
pub enum LoadedSession<E: Engine> {
    /// Between feeds.
    Idle(E::Repl),
    /// Mid-feed; the child re-announces the suspension.
    Suspended(E::Progress),
    /// A dump taken at completion, which the child reports as `Complete`.
    Complete {
        /// The session, ready for the next feed.
        repl: E::Repl,
        /// The completed snippet's result.
        value: MontyObject,
    },
}
