//! Host round-trips: a call to a host function, a lookup of an unknown name
//! and waiting on host futures each suspend the feed with a [`HostRequest`].
//! The whole interpreter parks (see [`Vm::park`]) while the host answers,
//! then the answer is pushed or thrown where the feed stopped and the feed
//! loop carries on.
//!
//! Suspending needs the Rust stack to hold nothing but the dispatch loop, so
//! a request made while a builtin runs a callback (`reentrancy > 0`) is an
//! error instead.

use std::collections::VecDeque;

use monty_types::{
    CallArgs, ExtFunctionResult, MontyException, MontyObject, NameLookupResult, PrintWriter, ResourceTracker,
    SourceRange, StackFrame, unstable::graph_parts,
};

use super::{
    Delivery, Frame, Session, Step, Vm,
    promise::{Job, Timer},
};
use crate::{
    error::{RunResult, Throw},
    heap::{FunctionObject, HeapId, PropertyKey},
    intern::AtomId,
    object_bridge::Importer,
    realm::ErrorKind,
    repl::exc_type_to_kind,
    value::JsValue,
};

/// What a suspended feed is waiting for.
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
}

/// The interpreter without its print sink, as a suspended feed keeps it
/// between the request and the host's answer (it must cross threads, which
/// a borrowed print callback cannot).
pub(crate) struct ParkedVm {
    session: Session,
    stack: Vec<JsValue>,
    frames: Vec<Frame>,
    pending: Option<JsValue>,
    traceback: Vec<StackFrame>,
    ticks: u8,
    reentrancy: usize,
    jobs: VecDeque<Job>,
    timers: Vec<Timer>,
    timer_clock: f64,
    timer_seq: u32,
    unhandled: Vec<HeapId>,
    module_async: bool,
    feed_result: Option<JsValue>,
    host_futures: Vec<(u32, HeapId)>,
}

impl ParkedVm {
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
            jobs,
            timers,
            timer_clock,
            timer_seq,
            unhandled,
            module_async,
            feed_result,
            host_futures,
        } = self;
        Vm {
            session,
            print,
            stack,
            frames,
            pending,
            traceback,
            ticks,
            reentrancy,
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
            jobs,
            timers,
            timer_clock,
            timer_seq,
            unhandled,
            module_async,
            feed_result,
            host_futures,
        } = self;
        ParkedVm {
            session,
            stack,
            frames,
            pending,
            traceback,
            ticks,
            reentrancy,
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
        if let Err(err) = self.check_can_suspend(name) {
            self.drop_all(JsValue::Undefined, args);
            return Err(err);
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

    fn check_can_suspend(&mut self, name: AtomId) -> RunResult<()> {
        if self.reentrancy == 0 {
            return Ok(());
        }
        let name = self.session.interns.atom(name).to_string_lossy();
        Err(self.type_error(&format!(
            "Host function '{name}' cannot be called from a callback run by a builtin"
        )))
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
