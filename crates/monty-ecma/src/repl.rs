//! The session API: feed JavaScript snippets one after another into a
//! persistent heap, realm and script scope.

use std::{error::Error, fmt, sync::Arc};

use monty_types::{
    CallArgs, ExcType, ExtFunctionResult, MontyException, MontyObject, NameLookupResult, NamedValues, OsPolicy,
    PrintWriter, ResourceError, ResourceLimits, ResourceTracker, SourceRange,
};
use oxc_allocator::Allocator;

use crate::{
    builtins,
    compile::compile,
    error::Throw,
    heap::{Heap, HeapId, ObjectClass},
    intern::{Interns, StaticAtom},
    parse::parse,
    realm::{ErrorKind, Realm, ScriptScope},
    resolve::Analysis,
    value::JsValue,
    vm::{
        Session, Vm,
        feed::FeedEnd,
        host::{HostRequest, ParkedVm},
    },
};

/// Heap entries reserved up front for the intrinsics.
const INITIAL_HEAP_CAPACITY: usize = 256;

/// A JavaScript session: feed snippets with [`feed`](Self::feed); bindings,
/// `globalThis` and the heap persist between them.
pub struct JsRepl {
    session: Session,
    os_policy: OsPolicy,
}

/// What a feed produced.
pub enum JsProgress {
    Complete {
        repl: JsRepl,
        value: MontyObject,
    },
    Failed {
        repl: JsRepl,
        error: MontyException,
    },
    /// The feed is waiting on the host; see [`JsSuspension`].
    Suspended(Box<JsSuspension>),
}

/// A feed parked on a host round-trip: the whole interpreter is kept, and
/// the matching `resume_*` method continues it with the host's answer.
pub struct JsSuspension {
    vm: ParkedVm,
    os_policy: OsPolicy,
    request: HostRequest,
}

/// What a suspended feed asks the host for.
pub enum JsSuspensionKind<'a> {
    /// A call of an undeclared name.
    FunctionCall {
        function_name: &'a str,
        args: &'a mut CallArgs,
        call_id: u32,
        position: &'a SourceRange,
    },
    /// A read of an undeclared name.
    NameLookup { name: &'a str, position: &'a SourceRange },
    /// Every queue is empty but promises wait on host futures.
    ResolveFutures {
        pending_call_ids: &'a [u32],
        position: &'a SourceRange,
    },
}

impl JsSuspension {
    /// What the feed is waiting for.
    pub fn kind(&mut self) -> JsSuspensionKind<'_> {
        match &mut self.request {
            HostRequest::FunctionCall {
                name,
                args,
                call_id,
                position,
            } => JsSuspensionKind::FunctionCall {
                function_name: name,
                args,
                call_id: *call_id,
                position,
            },
            HostRequest::NameLookup { name, position, .. } => JsSuspensionKind::NameLookup { name, position },
            HostRequest::ResolveFutures {
                pending_call_ids,
                position,
            } => JsSuspensionKind::ResolveFutures {
                pending_call_ids,
                position,
            },
        }
    }

    pub fn tracker(&self) -> &ResourceTracker {
        self.vm.tracker()
    }

    /// Answers a `FunctionCall`.
    pub fn resume_call(self, result: ExtFunctionResult, print: PrintWriter<'_>) -> JsProgress {
        self.resume(print, |vm| vm.apply_call_result(result))
    }

    /// Answers a `NameLookup`.
    pub fn resume_name_lookup(self, result: NameLookupResult, print: PrintWriter<'_>) -> JsProgress {
        let (atom, tolerant) = match &self.request {
            HostRequest::NameLookup { atom, tolerant, .. } => (*atom, *tolerant),
            _ => return self.abort(&protocol_misuse("a name lookup answer for another suspension"), print),
        };
        self.resume(print, |vm| vm.apply_name_lookup(atom, tolerant, result))
    }

    /// Delivers settled host futures to a `ResolveFutures` suspension.
    pub fn resume_futures(self, results: Vec<(u32, ExtFunctionResult)>, print: PrintWriter<'_>) -> JsProgress {
        self.resume(print, |vm| vm.apply_futures(results))
    }

    /// Throws `exc` where the feed suspended.
    pub fn abort(self, exc: &MontyException, print: PrintWriter<'_>) -> JsProgress {
        self.resume(print, |vm| {
            let error = vm.exception_value(exc);
            vm.throw_in_feed(error)
        })
    }

    /// Drops the feed, keeping the session for the next one.
    #[must_use]
    pub fn into_repl(self) -> JsRepl {
        JsRepl {
            session: self.vm.into_session(),
            os_policy: self.os_policy,
        }
    }

    fn resume(self, print: PrintWriter<'_>, apply: impl FnOnce(&mut Vm<'_>) -> Result<(), Throw>) -> JsProgress {
        let mut vm = self.vm.wake(print);
        vm.tracker().on_execution_start();
        let outcome = apply(&mut vm).and_then(|()| vm.continue_feed());
        settle(vm, self.os_policy, outcome)
    }
}

/// The exception for an answer that does not match the suspension.
fn protocol_misuse(what: &str) -> MontyException {
    MontyException::new(ExcType::RuntimeError, Some(format!("protocol misuse: {what}")))
}

#[derive(Debug)]
pub enum JsStartError {
    /// The intrinsics could not be built within the memory limit.
    Memory(String),
}

impl fmt::Display for JsStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory(message) => write!(f, "cannot create session: {message}"),
        }
    }
}

impl Error for JsStartError {}

impl JsRepl {
    /// Creates a session with its intrinsics; `script_name` labels tracebacks.
    pub fn new(script_name: &str, limits: ResourceLimits) -> Result<Self, JsStartError> {
        let tracker = ResourceTracker::new(limits);
        let heap = Heap::new(INITIAL_HEAP_CAPACITY, tracker);
        let session = Session {
            heap,
            interns: Interns::new(),
            realm: Realm::placeholder(),
            script: ScriptScope::default(),
            script_name: script_name.to_owned(),
            feed_count: 0,
            random_state: 0x9E37_79B9_7F4A_7C15,
            next_call_id: 1,
        };
        let mut vm = Vm::new(session, PrintWriter::Disabled);
        builtins::create_realm(&mut vm);
        let session = vm.into_session();
        session
            .heap
            .tracker
            .check_allocation(0)
            .map_err(|err| JsStartError::Memory(err.to_string()))?;
        Ok(Self {
            session,
            os_policy: OsPolicy::default(),
        })
    }

    /// Replaces the clock, sleep and random policies.
    // TODO(os-policy): `Date`, `Math.random` and timers read these once they exist.
    #[must_use]
    pub fn with_os_policy(mut self, os_policy: OsPolicy) -> Self {
        self.os_policy = os_policy;
        self
    }

    /// The session's resource tracker.
    pub fn tracker(&self) -> &ResourceTracker {
        &self.session.heap.tracker
    }

    /// Mutable access to the tracker, for tightening limits on restore.
    pub fn tracker_mut(&mut self) -> &mut ResourceTracker {
        &mut self.session.heap.tracker
    }

    /// The filename the next feed's frames carry.
    #[must_use]
    pub fn next_script_name(&self) -> String {
        if self.session.feed_count == 0 {
            self.session.script_name.clone()
        } else {
            format!("<javascript-input-{}>", self.session.feed_count)
        }
    }

    /// Rejects a snippet before parsing: nothing to check yet beyond size.
    pub fn check_source(&self, code: &str) -> Result<(), MontyException> {
        if code.len() > u32::MAX as usize / 2 {
            return Err(MontyException::new(
                ExcType::SyntaxError,
                Some("source is too large".to_owned()),
            ));
        }
        Ok(())
    }

    /// Compiles and runs `code`, with `inputs` defined on `globalThis` first.
    pub fn feed(mut self, code: &str, inputs: &NamedValues, print: PrintWriter<'_>) -> JsProgress {
        let script_name = self.next_script_name();
        let feed = self.session.feed_count;
        self.session.feed_count += 1;
        self.session.interns.add_source(code);
        let allocator = Allocator::default();
        let compiled = match parse(&allocator, code) {
            Ok((program, scoping)) => {
                let analysis = Analysis::new(&program, scoping);
                compile(
                    &program,
                    &analysis,
                    feed,
                    &mut self.session.interns,
                    &mut self.session.script,
                )
            }
            Err(err) => Err(err),
        };
        let compiled = match compiled {
            Ok(compiled) => compiled,
            Err(err) => {
                return JsProgress::Failed {
                    repl: self,
                    error: err.into_exception(&script_name, code),
                };
            }
        };
        for function in compiled.functions {
            self.session.interns.add_function(function);
        }
        let module_id = self.session.interns.add_function(compiled.module);
        let module = Arc::clone(self.session.interns.function(module_id));
        self.session.script_name = script_name;
        let mut vm = Vm::new(self.session, print);
        vm.tracker().on_feed_start();
        vm.tracker().on_execution_start();
        let outcome = vm.import_inputs(inputs).and_then(|()| vm.start_feed(module));
        settle(vm, self.os_policy, outcome)
    }
}

/// Turns how a feed left the interpreter into the progress the host sees.
fn settle(mut vm: Vm<'_>, os_policy: OsPolicy, outcome: Result<FeedEnd, Throw>) -> JsProgress {
    let result = match outcome {
        Ok(FeedEnd::Suspended(request)) => {
            vm.tracker().on_execution_stop();
            return JsProgress::Suspended(Box::new(JsSuspension {
                vm: vm.park(),
                os_policy,
                request,
            }));
        }
        Ok(FeedEnd::Complete(value)) => {
            let exported = vm.export(&value);
            value.drop_with(&mut vm.session.heap);
            match exported {
                Ok(exported) => Ok(exported),
                Err(throw) => Err(throw_to_exception(&mut vm, throw)),
            }
        }
        Err(throw) => Err(throw_to_exception(&mut vm, throw)),
    };
    vm.tracker().on_execution_stop();
    let repl = JsRepl {
        session: vm.into_session(),
        os_policy,
    };
    match result {
        Ok(value) => JsProgress::Complete { repl, value },
        Err(error) => JsProgress::Failed { repl, error },
    }
}

fn throw_to_exception(vm: &mut Vm<'_>, throw: Throw) -> MontyException {
    let traceback = vm.take_traceback();
    match throw {
        Throw::Resource(err) => {
            let exc_type = match err {
                ResourceError::Time { .. } => ExcType::TimeoutError,
                ResourceError::Memory { .. } => ExcType::MemoryError,
                ResourceError::Recursion { .. } => ExcType::RecursionError,
            };
            MontyException::with_traceback(exc_type, Some(err.to_string()), traceback)
        }
        Throw::Value(value) => {
            let (exc_type, message) = match &value {
                JsValue::Ref(id) if matches!(vm.object(*id).map(|o| &o.class), Some(ObjectClass::Error)) => {
                    let message = vm.get_atom(&value, StaticAtom::Message.id()).ok();
                    let text = message
                        .as_ref()
                        .and_then(|m| vm.string_of(m))
                        .map(|s| s.to_string_lossy());
                    if let Some(message) = message {
                        message.drop_with(&mut vm.session.heap);
                    }
                    (error_exc_type(vm, *id), text.unwrap_or_default())
                }
                other => {
                    let shown = vm.inspect(other).unwrap_or_else(|_| "<value>".to_owned());
                    (ExcType::Error, format!("Uncaught {shown}"))
                }
            };
            value.drop_with(&mut vm.session.heap);
            MontyException::with_traceback(exc_type, Some(message), traceback)
        }
    }
}

/// The host-visible exception type of an error object: its nearest builtin
/// error prototype; a user subclass of `Error` reports as `Error`.
pub(crate) fn error_exc_type(vm: &Vm<'_>, error: HeapId) -> ExcType {
    let mut current = vm.get_prototype(error);
    while let Some(proto) = current {
        if let Some(kind) = vm.realm().error_kind_of(proto) {
            return kind_to_exc_type(kind);
        }
        current = vm.get_prototype(proto);
    }
    ExcType::Error
}

fn kind_to_exc_type(kind: ErrorKind) -> ExcType {
    match kind {
        ErrorKind::Error => ExcType::Error,
        ErrorKind::TypeError => ExcType::TypeError,
        ErrorKind::RangeError => ExcType::RangeError,
        ErrorKind::ReferenceError => ExcType::ReferenceError,
        ErrorKind::SyntaxError => ExcType::SyntaxError,
        ErrorKind::EvalError => ExcType::EvalError,
        ErrorKind::UriError => ExcType::UriError,
        ErrorKind::AggregateError => ExcType::AggregateError,
    }
}

/// The error class a host exception materializes as.
pub(crate) fn exc_type_to_kind(exc_type: ExcType) -> ErrorKind {
    match exc_type {
        ExcType::TypeError => ErrorKind::TypeError,
        ExcType::RangeError | ExcType::RecursionError | ExcType::MemoryError => ErrorKind::RangeError,
        ExcType::ReferenceError | ExcType::NameError | ExcType::UnboundLocalError => ErrorKind::ReferenceError,
        ExcType::SyntaxError => ErrorKind::SyntaxError,
        ExcType::EvalError => ErrorKind::EvalError,
        ExcType::UriError => ErrorKind::UriError,
        ExcType::AggregateError => ErrorKind::AggregateError,
        _ => ErrorKind::Error,
    }
}
