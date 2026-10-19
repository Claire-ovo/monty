//! Host round-trips: a call to a host function, a lookup of an unknown name
//! and waiting on host futures each suspend the feed with a [`HostRequest`].
//! The whole interpreter parks (see [`Vm::park`]) while the host answers,
//! then the answer is pushed or thrown where the feed stopped and the feed
//! loop carries on.
//!
//! Suspending needs the Rust stack to hold nothing but the dispatch loop, so
//! a request made while a builtin runs a callback (`reentrancy > 0`) is an
//! error instead.

use std::{collections::VecDeque, sync::Arc};

use monty_types::{
    CallArgs, ExtFunctionResult, MontyException, MontyObject, NameLookupResult, OsFunctionCall, PrintWriter,
    ResourceTracker, SourceRange, StackFrame, unstable::graph_parts,
};
use serde::{Deserialize, Serialize};

use super::{
    Completion, Construct, Delivery, Frame, Session, Step, Vm,
    promise::{Job, Timer},
};
use crate::{
    builtins::date,
    error::{RunResult, Throw},
    heap::{FunctionObject, HeapId, PropertyKey},
    intern::{AtomId, FunctionId},
    object_bridge::Importer,
    realm::ErrorKind,
    repl::exc_type_to_kind,
    value::JsValue,
};

/// What a suspended feed is waiting for.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum HostRequest {
    /// `name(...args)` on an undeclared name.
    FunctionCall {
        name: String,
        args: CallArgs,
        call_id: u32,
        position: SourceRange,
    },
    /// A read of an undeclared name; `tolerant` for `typeof name`, where an
    /// unknown name is `undefined` rather than a `ReferenceError`.
    NameLookup {
        name: String,
        atom: AtomId,
        tolerant: bool,
        position: SourceRange,
    },
    /// Every queue is empty but promises still wait on host futures.
    ResolveFutures {
        pending_call_ids: Vec<u32>,
        position: SourceRange,
    },
    /// An OS operation the policy routes to the host (the clock).
    OsCall {
        call: OsFunctionCall,
        call_id: u32,
        position: SourceRange,
        /// What the suspended builtin makes of the answer.
        answer: OsAnswer,
    },
}

/// How a host clock answer (seconds since the epoch) becomes the suspended
/// builtin's result.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) enum OsAnswer {
    /// `Date.now()`: milliseconds.
    ClockMillis,
    /// `new Date()`: a `Date` object.
    NewDate,
    /// `Date()`: the date's string.
    DateString,
}

/// The interpreter without its print sink, as a suspended feed keeps it
/// between the request and the host's answer (it must cross threads, which
/// a borrowed print callback cannot) and as a dump stores it.
#[derive(Serialize, Deserialize)]
pub(crate) struct ParkedVm {
    session: Session,
    stack: Vec<JsValue>,
    frames: Vec<SerializedFrame>,
    pending: Option<JsValue>,
    traceback: Vec<StackFrame>,
    ticks: u8,
    reentrancy: usize,
    native_depth: usize,
    jobs: VecDeque<Job>,
    timers: Vec<Timer>,
    timer_clock: f64,
    timer_seq: u32,
    unhandled: Vec<HeapId>,
    module_async: bool,
    feed_result: Option<JsValue>,
    host_futures: Vec<(u32, HeapId)>,
}

/// A frame as a dump stores it: the function by id rather than by its
/// shared code.
#[derive(Serialize, Deserialize)]
pub(crate) struct SerializedFrame {
    function: FunctionId,
    ip: usize,
    op_start: usize,
    base: usize,
    operands: usize,
    callee: Option<HeapId>,
    new_target: JsValue,
    construct: Construct,
    generator: Option<HeapId>,
    completion: Completion,
}

impl ParkedVm {
    /// Rebuilds the indexes a dump leaves out; call once after deserializing.
    pub(crate) fn restore(&mut self) {
        self.session.restore();
    }

    pub(crate) fn tracker_mut(&mut self) -> &mut ResourceTracker {
        &mut self.session.heap.tracker
    }

    pub(crate) fn script_name(&self) -> &str {
        &self.session.script_name
    }

    /// Reattaches the interpreter to a print sink.
    pub(crate) fn wake(self, print: PrintWriter<'_>) -> Vm<'_> {
        let Self {
            session,
            stack,
            frames,
            pending,
            traceback,
            ticks,
            reentrancy,
            native_depth,
            jobs,
            timers,
            timer_clock,
            timer_seq,
            unhandled,
            module_async,
            feed_result,
            host_futures,
        } = self;
        let frames = frames
            .into_iter()
            .map(|frame| Frame {
                function: Arc::clone(session.interns.function(frame.function)),
                ip: frame.ip,
                op_start: frame.op_start,
                base: frame.base,
                operands: frame.operands,
                callee: frame.callee,
                new_target: frame.new_target,
                construct: frame.construct,
                generator: frame.generator,
                completion: frame.completion,
            })
            .collect();
        Vm {
            session,
            print,
            stack,
            frames,
            pending,
            traceback,
            ticks,
            reentrancy,
            native_depth,
            jobs,
            timers,
            timer_clock,
            timer_seq,
            unhandled,
            module_async,
            feed_result,
            host_futures,
        }
    }

    /// Drops the parked feed, keeping the session.
    pub(crate) fn into_session(self) -> Session {
        self.wake(PrintWriter::Disabled).into_session()
    }

    pub(crate) fn tracker(&self) -> &ResourceTracker {
        &self.session.heap.tracker
    }
}

impl Vm<'_> {
    /// Detaches the interpreter from its print sink for the host round-trip.
    pub(crate) fn park(self) -> ParkedVm {
        let Vm {
            session,
            print: _,
            stack,
            frames,
            pending,
            traceback,
            ticks,
            reentrancy,
            native_depth,
            jobs,
            timers,
            timer_clock,
            timer_seq,
            unhandled,
            module_async,
            feed_result,
            host_futures,
        } = self;
        let frames = frames
            .into_iter()
            .map(|frame| SerializedFrame {
                function: frame.function.id,
                ip: frame.ip,
                op_start: frame.op_start,
                base: frame.base,
                operands: frame.operands,
                callee: frame.callee,
                new_target: frame.new_target,
                construct: frame.construct,
                generator: frame.generator,
                completion: frame.completion,
            })
            .collect();
        ParkedVm {
            session,
            stack,
            frames,
            pending,
            traceback,
            ticks,
            reentrancy,
            native_depth,
            jobs,
            timers,
            timer_clock,
            timer_seq,
            unhandled,
            module_async,
            feed_result,
            host_futures,
        }
    }

    // === Raising requests ===

    /// A host function object for an undeclared name (one owned reference).
    pub(crate) fn external_function(&mut self, name: AtomId) -> JsValue {
        JsValue::Ref(self.new_function_object(FunctionObject::External(name), name, 0))
    }

    /// Calling a host function: exports the arguments and suspends, unless a
    /// builtin's callback is on the Rust stack, which cannot suspend.
    pub(super) fn external_call(&mut self, name: AtomId, args: Vec<JsValue>) -> RunResult<Step> {
        if self.reentrancy > 0 {
            self.drop_all(JsValue::Undefined, args);
            let text = self.session.interns.atom(name).to_string_lossy();
            return Err(self.type_error(&format!(
                "Host function '{text}' cannot be called from a callback run by a builtin"
            )));
        }
        let mut call_args = CallArgs::new();
        let mut exported = Ok(());
        for arg in &args {
            if exported.is_ok() {
                exported = self.export(arg).map(|value| call_args.push_arg(value));
            }
        }
        self.drop_all(JsValue::Undefined, args);
        exported?;
        let call_id = self.session.next_call_id;
        self.session.next_call_id += 1;
        Ok(Step::Suspend(HostRequest::FunctionCall {
            name: self.session.interns.atom(name).to_string_lossy(),
            args: call_args,
            call_id,
            position: self.current_position(),
        }))
    }

    /// Reading an undeclared name: asks the host, or (where no suspension is
    /// possible) resolves it as unknown right away.
    pub(super) fn name_lookup(&mut self, atom: AtomId, tolerant: bool) -> RunResult<Step> {
        if self.reentrancy > 0 {
            return if tolerant {
                self.stack.push(JsValue::Undefined);
                Ok(Step::Continue)
            } else {
                Err(self.reference_error(atom))
            };
        }
        Ok(Step::Suspend(HostRequest::NameLookup {
            name: self.session.interns.atom(atom).to_string_lossy(),
            atom,
            tolerant,
            position: self.current_position(),
        }))
    }

    /// A builtin's request for an OS operation: the throw that suspends the
    /// feed when the builtin was called from the dispatch loop.
    pub(crate) fn os_call(&mut self, call: OsFunctionCall, answer: OsAnswer) -> Throw {
        let call_id = self.session.next_call_id;
        self.session.next_call_id += 1;
        Throw::Suspend(Box::new(HostRequest::OsCall {
            call,
            call_id,
            position: self.current_position(),
            answer,
        }))
    }

    /// The `TypeError` for a request made where nothing can suspend.
    pub(crate) fn suspend_blocked(&mut self, request: &HostRequest) -> Throw {
        let message = match request {
            HostRequest::FunctionCall { name, .. } => {
                format!("Host function '{name}' cannot be called from a callback run by a builtin")
            }
            HostRequest::OsCall { .. } => "The host clock cannot be read from a callback run by a builtin".to_owned(),
            HostRequest::NameLookup { name, .. } => {
                format!("'{name}' cannot be looked up on the host from a callback run by a builtin")
            }
            HostRequest::ResolveFutures { .. } => "cannot wait for host futures here".to_owned(),
        };
        self.type_error(&message)
    }

    /// The request a feed whose queues ran dry raises for its host futures.
    pub(super) fn resolve_futures_request(&self) -> HostRequest {
        HostRequest::ResolveFutures {
            pending_call_ids: self.host_futures.iter().map(|(id, _)| *id).collect(),
            position: SourceRange {
                filename: self.session.script_name.clone(),
                start: 0,
                end: 0,
            },
        }
    }

    /// Where the running instruction is, for the host's error reporting.
    fn current_position(&self) -> SourceRange {
        let (start, end) = self
            .frames
            .last()
            .and_then(|frame| frame.function.code.location(frame.op_start))
            .unwrap_or((0, 0));
        SourceRange {
            filename: self.session.script_name.clone(),
            start,
            end,
        }
    }

    // === Applying answers ===

    /// Delivers the host's answer to a `FunctionCall`: the call expression's
    /// value, a throw at the call, or a promise for a future.
    pub(crate) fn apply_call_result(&mut self, result: ExtFunctionResult) -> RunResult<()> {
        match result {
            ExtFunctionResult::Return(value) => {
                let value = self.import_value(&value)?;
                self.stack.push(value);
                Ok(())
            }
            ExtFunctionResult::Future(call_id) => {
                let promise = self.new_promise();
                self.session.heap.inc_ref(promise);
                self.host_futures.push((call_id, promise));
                self.stack.push(JsValue::Ref(promise));
                Ok(())
            }
            ExtFunctionResult::Error(exc) => {
                let error = self.exception_value(&exc);
                self.throw_in_feed(error)
            }
            ExtFunctionResult::NotFound(name) => {
                let error = self.new_error(ErrorKind::ReferenceError, &format!("{name} is not defined"));
                self.throw_in_feed(error)
            }
        }
    }

    /// Delivers the host's answer to an OS call: the clock's seconds become
    /// the value the suspended builtin would have produced.
    pub(crate) fn apply_os_result(&mut self, result: ExtFunctionResult, answer: OsAnswer) -> RunResult<()> {
        match result {
            ExtFunctionResult::Return(value) => {
                let value = self.import_value(&value)?;
                let seconds = self.coerce_number(&value);
                value.drop_with(&mut self.session.heap);
                let produced = date::from_host_seconds(self, seconds?, answer)?;
                self.stack.push(produced);
                Ok(())
            }
            ExtFunctionResult::Future(_) => {
                let error = self.new_error(ErrorKind::TypeError, "the host clock cannot be answered with a future");
                self.throw_in_feed(error)
            }
            ExtFunctionResult::Error(exc) => {
                let error = self.exception_value(&exc);
                self.throw_in_feed(error)
            }
            ExtFunctionResult::NotFound(what) => {
                let error = self.new_error(ErrorKind::Error, &format!("the host does not provide {what}"));
                self.throw_in_feed(error)
            }
        }
    }

    /// Delivers the host's answer to a `NameLookup`: a found value is also
    /// stored on `globalThis`, so the name does not ask again.
    pub(crate) fn apply_name_lookup(
        &mut self,
        atom: AtomId,
        tolerant: bool,
        result: NameLookupResult,
    ) -> RunResult<()> {
        match result {
            NameLookupResult::Value(value) => {
                let value = self.import_value(&value)?;
                let global = self.session.realm.global;
                self.define_data(
                    global,
                    PropertyKey::Atom(atom),
                    value.clone_with_heap(&self.session.heap),
                );
                self.stack.push(value);
                Ok(())
            }
            NameLookupResult::Undefined if tolerant => {
                self.stack.push(JsValue::Undefined);
                Ok(())
            }
            NameLookupResult::Undefined => {
                let error = self.reference_error(atom);
                let Throw::Value(error) = error else {
                    return Err(error);
                };
                self.throw_in_feed(error)
            }
            NameLookupResult::Error(exc) => {
                let error = self.exception_value(&exc);
                self.throw_in_feed(error)
            }
        }
    }

    /// Settles the promises of host futures the host has resolved; ids it
    /// does not name stay pending.
    pub(crate) fn apply_futures(&mut self, results: Vec<(u32, ExtFunctionResult)>) -> RunResult<()> {
        for (call_id, result) in results {
            let Some(index) = self.host_futures.iter().position(|(id, _)| *id == call_id) else {
                continue;
            };
            let (_, promise) = self.host_futures.swap_remove(index);
            let outcome = match result {
                ExtFunctionResult::Return(value) => match self.import_value(&value) {
                    Ok(value) => self.resolve_promise(promise, value),
                    Err(err) => Err(err),
                },
                ExtFunctionResult::Error(exc) => {
                    let error = self.exception_value(&exc);
                    self.reject_promise(promise, error);
                    Ok(())
                }
                ExtFunctionResult::NotFound(name) => {
                    let error = self.new_error(ErrorKind::ReferenceError, &format!("{name} is not defined"));
                    self.reject_promise(promise, error);
                    Ok(())
                }
                ExtFunctionResult::Future(_) => {
                    self.host_futures.push((call_id, promise));
                    continue;
                }
            };
            self.session.heap.dec_ref(promise);
            outcome?;
        }
        Ok(())
    }

    /// Throws `error` where the feed stopped: into the suspended frame's
    /// handlers, or out of the feed when nothing catches it.
    pub(crate) fn throw_in_feed(&mut self, error: JsValue) -> RunResult<()> {
        match self.handle_throw(Throw::Value(error), 0)? {
            None => Ok(()),
            Some(Delivery::Value(value)) => {
                if self.feed_result.is_none() {
                    self.feed_result = Some(value);
                } else {
                    value.drop_with(&mut self.session.heap);
                }
                Ok(())
            }
            Some(Delivery::Nothing) => Ok(()),
            Some(Delivery::Yielded(value) | Delivery::Propagate(value)) => {
                value.drop_with(&mut self.session.heap);
                Err(self.internal_error("unexpected delivery after a host throw"))
            }
        }
    }

    /// A host exception as a JavaScript error of the matching class.
    pub(crate) fn exception_value(&mut self, exc: &MontyException) -> JsValue {
        let kind = exc_type_to_kind(exc.exc_type());
        self.new_error(kind, exc.message().unwrap_or_default())
    }

    /// Imports one host value.
    pub(crate) fn import_value(&mut self, value: &MontyObject) -> RunResult<JsValue> {
        let (graph, root) = graph_parts(value);
        let mut importer = Importer::new(graph);
        importer.value(self, root)
    }
}
