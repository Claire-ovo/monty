//! The Python interpreter as a protocol [`Engine`]: `MontyRepl` sessions, plus
//! the type-checking flow (`ty` over the session's accumulated snippets, with
//! host modules' stubs fetched through `import_stubs`) that only Python has.

use std::mem;

use monty::{
    Dump, MontyRepl, ReplProgress, ReplStartError, Session, SessionRef as MontySessionRef, dump,
    source_within_nesting_bound,
};
use monty_type_checking::{SourceFile, TypeCheckContext, TypeChecker, scan_imports};
use monty_types::{
    AssertMessageAnnotations, CHECKER_MODULE_NAMES, CompileOptions, ExtFunctionResult, IMPORT_STUBS_CALL_ID, Language,
    ModuleStub, MontyException, MontyObject, NameLookupResult, NamedValues, OsFunctionCall, OsPolicy, PrintWriter,
    ResourceLimits, ResourceTracker, SANDBOX_MODULE_NAMES, SOURCE_SCAN_THRESHOLD, SourceRange, TypeCheckState,
    TypeCheckingConfig, allocate_into_baseline,
};

use super::{
    Child, Engine, Failed, Loaded, LoadedSession, Outcome, PreFeed, SessionRef, Step, Suspension, error_event_from,
    event, protocol_violation,
};
use crate::{os_call_to_proto, pb};

/// The protocol child over the Python interpreter.
pub type PythonChild = Child<PythonEngine>;

/// The Python interpreter's [`Engine`].
#[derive(Default)]
pub struct PythonEngine {
    /// Built by the first type-checked feed and kept for the worker's life; what
    /// building it allocates joins the baseline rather than that session's budget.
    /// What its first run caches is charged to the session, as it always was.
    type_checker: Option<TypeChecker>,
    /// `Some` when the session was created with `type_check: true`.
    type_check: Option<TypeCheckState>,
    /// Host module stubs received through `import_stubs` or restored by `Load`.
    module_stubs: Vec<ModuleStub>,
    /// The session's `Configure` fields this engine owns, materialized by
    /// [`create_repl`](Engine::create_repl).
    config: Option<SessionConfig>,
    /// Modules a parked feed asked the host for, in source order.
    asked: Vec<String>,
}

/// The Python-specific session settings from `Configure`.
struct SessionConfig {
    type_check: bool,
    type_check_stubs: Option<String>,
    checking: TypeCheckingConfig,
    options: CompileOptions,
}

impl Engine for PythonEngine {
    type Repl = MontyRepl;
    type Progress = ReplProgress;
    type Restored = (Option<TypeCheckState>, Vec<ModuleStub>);

    const LANGUAGES: &'static [Language] = &[Language::Python];

    fn configure(&mut self, configure: &pb::Configure) -> Result<(), Box<pb::ChildEvent>> {
        // ty parses the stubs with every feed, unguarded.
        if let Some(stubs) = &configure.type_check_stubs
            && !source_within_nesting_bound(stubs, SOURCE_SCAN_THRESHOLD)
        {
            return Err(Box::new(protocol_violation(
                "invalid type_check_stubs: Source is too deeply nested",
            )));
        }
        self.config = Some(SessionConfig {
            type_check: configure.type_check,
            type_check_stubs: configure.type_check_stubs.clone(),
            checking: TypeCheckingConfig::from(configure),
            // Missing field means an older parent; the feature defaults to on.
            options: CompileOptions {
                assert_message_annotations: configure.assert_message_annotations.map_or_else(
                    AssertMessageAnnotations::default,
                    AssertMessageAnnotations::from_max_bytes,
                ),
                // Not on the wire: every host gets the default.
                source_scan_threshold: SOURCE_SCAN_THRESHOLD,
            },
        });
        Ok(())
    }

    fn type_checks(&self) -> bool {
        self.type_check.is_some()
    }

    fn create_repl(
        &mut self,
        script_name: &str,
        limits: ResourceLimits,
        os_policy: OsPolicy,
    ) -> Result<MontyRepl, Box<pb::ChildEvent>> {
        let config = self.config.take().unwrap_or_default();
        self.type_check = config.type_check.then(|| TypeCheckState {
            committed_stubs: config.type_check_stubs.unwrap_or_default(),
            pending_snippet: None,
            config: config.checking,
            committed_imports: String::new(),
            pending_imports: String::new(),
            asked_modules: Vec::new(),
        });
        Ok(MontyRepl::new(script_name, ResourceTracker::new(limits), config.options).with_os_policy(os_policy))
    }

    fn check_source(repl: &MontyRepl, code: &str) -> Result<(), MontyException> {
        repl.check_source(code).map(|_| ())
    }

    fn set_cwd(repl: &mut MontyRepl, cwd: &str) {
        repl.set_cwd(cwd);
    }

    /// Snippets fed with `skip_type_check` never become type-check context: the
    /// caller explicitly excluded them from checking, so later snippets must not
    /// be checked against their (unchecked) bindings either.
    fn pre_feed(&mut self, repl: &MontyRepl, script_name: &str, code: &str, type_check: bool) -> PreFeed {
        let Some(state) = self.type_check.as_mut().filter(|_| type_check) else {
            return PreFeed::Run;
        };
        let imports = scan_imports(code);
        state.pending_snippet = Some(code.to_owned());
        state.pending_imports = imports.prelude;
        // Each answer is final for the session, even if it contains no stub.
        let unasked = |name: &str| {
            !SANDBOX_MODULE_NAMES.contains(&name)
                && !CHECKER_MODULE_NAMES.contains(&name)
                && !state.asked_modules.iter().any(|asked| asked == name)
        };
        if let Some(first) = imports.modules.iter().find(|module| unasked(&module.name)) {
            let asked: Vec<String> = imports
                .modules
                .iter()
                .filter(|module| unasked(&module.name))
                .map(|module| module.name.clone())
                .collect();
            let position = SourceRange::new(
                &repl.next_script_name(),
                first.range.start().to_u32(),
                first.range.end().to_u32(),
            );
            let ask = OsFunctionCall::ImportStubs(asked.clone());
            self.asked = asked;
            return PreFeed::Ask(event(pb::child_event::Kind::OsCall(os_call_to_proto(
                IMPORT_STUBS_CALL_ID,
                ask,
                false,
                &position,
            ))));
        }
        self.type_check_feed(script_name, code)
    }

    /// Records the stubs and type checks; an exception answer ends the feed before execution.
    fn resume_pre_feed(
        &mut self,
        _repl: &MontyRepl,
        script_name: &str,
        code: &str,
        answer: Option<ExtFunctionResult>,
    ) -> PreFeed {
        let stubs = match answer {
            None | Some(ExtFunctionResult::NotFound(_)) => Vec::new(),
            Some(ExtFunctionResult::Return(answer)) => match stubs_from_answer(&answer, &self.asked) {
                Ok(stubs) => stubs,
                Err(message) => return PreFeed::Invalid(protocol_violation(&message)),
            },
            Some(ExtFunctionResult::Future(_)) => {
                return PreFeed::Invalid(protocol_violation("import_stubs cannot be answered with a future"));
            }
            Some(ExtFunctionResult::Error(exc)) => return PreFeed::Reject(error_event_from(&exc)),
        };
        for stub in stubs {
            if !self.module_stubs.iter().any(|known| known.module() == stub.module()) {
                self.module_stubs.push(stub);
            }
        }
        let asked = mem::take(&mut self.asked);
        if let Some(state) = &mut self.type_check {
            state.asked_modules.extend(asked);
        }
        self.type_check_feed(script_name, code)
    }

    fn feed_start(&mut self, repl: MontyRepl, code: &str, inputs: NamedValues, print: PrintWriter<'_>) -> Step<Self> {
        // already found within the bound by `check_source`, so this cannot fail
        let checked = match repl.check_source(code) {
            Ok(checked) => checked,
            Err(error) => return Err(Box::new(Failed { repl, error })),
        };
        step(repl.feed_start_checked(checked, inputs, print))
    }

    fn feed_settled(&mut self, admitted: bool) {
        let Some(state) = &mut self.type_check else {
            return;
        };
        let Some(snippet) = state.pending_snippet.take() else {
            state.pending_imports.clear();
            return;
        };
        if !admitted {
            state.pending_imports.clear();
            return;
        }
        // The stubs' star import does not re-export imports. Keep them separately,
        // deduplicated and ordered so the latest binding wins.
        let imports = mem::take(&mut state.pending_imports);
        let mut committed: Vec<&str> = state.committed_imports.lines().collect();
        for import in imports.lines() {
            committed.retain(|line| *line != import);
            committed.push(import);
        }
        let mut joined = committed.join("\n");
        if !joined.is_empty() {
            joined.push('\n');
        }
        state.committed_imports = joined;
        state.committed_stubs.push('\n');
        state.committed_stubs.push_str(&snippet);
    }

    fn resume_call(&mut self, progress: ReplProgress, result: ExtFunctionResult, print: PrintWriter<'_>) -> Step<Self> {
        step(match progress {
            ReplProgress::FunctionCall(call) => call.resume(result, print),
            ReplProgress::OsCall(call) => call.resume(result, print),
            _ => unreachable!("the child only answers a function or OS call with ResumeCall"),
        })
    }

    fn resume_eager(
        &mut self,
        progress: ReplProgress,
        result: Result<MontyObject, MontyException>,
        print: PrintWriter<'_>,
    ) -> Step<Self> {
        step(match progress {
            ReplProgress::FunctionCall(call) => call.resume_eager(result, print),
            ReplProgress::OsCall(call) => call.resume_eager(result, print),
            _ => unreachable!("the child only answers eagerly a call that allowed it"),
        })
    }

    fn resume_name_lookup(
        &mut self,
        progress: ReplProgress,
        result: NameLookupResult,
        print: PrintWriter<'_>,
    ) -> Step<Self> {
        let ReplProgress::NameLookup(lookup) = progress else {
            unreachable!("the child only answers a name lookup with ResumeNameLookup")
        };
        step(lookup.resume(result, print))
    }

    fn resume_futures(
        &mut self,
        progress: ReplProgress,
        results: Vec<(u32, ExtFunctionResult)>,
        print: PrintWriter<'_>,
    ) -> Step<Self> {
        let ReplProgress::ResolveFutures(state) = progress else {
            unreachable!("the child only batches results into a ResolveFutures suspension")
        };
        step(state.resume(results, print))
    }

    fn abort(&mut self, progress: ReplProgress, exc: MontyException, print: PrintWriter<'_>) -> Step<Self> {
        step(match progress {
            ReplProgress::FunctionCall(call) => call.abort(exc, print),
            ReplProgress::OsCall(call) => call.abort(exc, print),
            ReplProgress::NameLookup(lookup) => lookup.abort(exc, print),
            ReplProgress::ResolveFutures(state) => state.abort(exc, print),
            ReplProgress::Complete { .. } => unreachable!("a completed feed is never stored as a suspension"),
        })
    }

    fn suspension(progress: &mut ReplProgress) -> Suspension<'_> {
        match progress {
            ReplProgress::FunctionCall(call) => Suspension::FunctionCall {
                function_name: &call.function_name,
                args: &mut call.args,
                call_id: call.call_id,
                object_id: call.object_id,
                allow_eager_await: call.allow_eager_await,
                position: &call.position,
            },
            ReplProgress::OsCall(call) => Suspension::OsCall {
                call: &mut call.function_call,
                call_id: call.call_id,
                allow_eager_await: call.allow_eager_await,
                position: &call.position,
            },
            ReplProgress::NameLookup(lookup) => Suspension::NameLookup {
                name: &lookup.name,
                object_id: lookup.object_id(),
                position: &lookup.position,
            },
            ReplProgress::ResolveFutures(state) => Suspension::ResolveFutures {
                pending_call_ids: state.pending_call_ids(),
                position: state.position(),
            },
            ReplProgress::Complete { .. } => unreachable!("a completed feed is never stored as a suspension"),
        }
    }

    fn into_repl(progress: ReplProgress) -> MontyRepl {
        progress.into_repl()
    }

    fn repl_tracker(repl: &MontyRepl) -> &ResourceTracker {
        repl.tracker()
    }

    fn progress_tracker(progress: &ReplProgress) -> &ResourceTracker {
        progress.tracker()
    }

    fn dump(&self, script_name: &str, session: SessionRef<'_, Self>) -> Result<Vec<u8>, String> {
        let session = match session {
            SessionRef::Idle(repl) => MontySessionRef::Idle(repl),
            SessionRef::Suspended(progress) => MontySessionRef::Suspended(progress),
        };
        dump(script_name, self.type_check.as_ref(), &self.module_stubs, session).map_err(|err| err.to_string())
    }

    fn load(&self, state: &[u8], limits: &ResourceLimits) -> Result<Loaded<Self>, Box<pb::ChildEvent>> {
        let Dump {
            script_name,
            type_check,
            module_stubs,
            mut state,
        } = Dump::load(state).map_err(|err| Box::new(protocol_violation(&format!("failed to load session: {err}"))))?;
        // Retain snapshot clocks and stricter limits across both idle and suspended restores.
        match &mut state {
            Session::Idle(repl) => repl.tracker_mut().tighten_limits(limits),
            Session::Suspended(progress) => progress.tracker_mut().tighten_limits(limits),
            Session::Running(_) => {}
        }
        let session = match state {
            Session::Idle(repl) => LoadedSession::Idle(*repl),
            // the protocol only ever serves repl sessions; a `MontyRun`
            // execution has no way to accept further feeds
            Session::Running(_) => {
                return Err(Box::new(protocol_violation(
                    "dump holds a one-shot run, not a repl session",
                )));
            }
            Session::Suspended(progress) => match *progress {
                // The public Rust dump API can serialize Complete, even though
                // this worker only dumps idle or suspended sessions.
                ReplProgress::Complete { repl, value } => LoadedSession::Complete { repl, value },
                progress => LoadedSession::Suspended(progress),
            },
        };
        Ok(Loaded {
            script_name,
            session,
            restored: (type_check, module_stubs),
        })
    }

    fn adopt(&mut self, (type_check, module_stubs): Self::Restored) {
        // the dump's stubs, not this worker's `Configure`: the session was checked against them
        self.type_check = type_check;
        self.module_stubs = module_stubs;
    }

    fn reset(&mut self) -> Result<(), String> {
        self.type_check = None;
        self.module_stubs = Vec::new();
        self.config = None;
        self.asked = Vec::new();
        self.type_checker.as_mut().map_or(Ok(()), TypeChecker::reset)
    }
}

impl PythonEngine {
    /// Checks against accumulated stubs and imports: the feed runs when the
    /// snippet passes and is rejected with the rendered diagnostics otherwise.
    fn type_check_feed(&mut self, script_name: &str, code: &str) -> PreFeed {
        let Some(state) = self.type_check.as_ref() else {
            return PreFeed::Run;
        };
        let stubs =
            (!state.committed_stubs.is_empty()).then(|| SourceFile::new(&state.committed_stubs, "repl_type_stubs.pyi"));
        let context = TypeCheckContext {
            stubs: stubs.as_ref(),
            module_stubs: &self.module_stubs,
            prelude: &state.committed_imports,
        };
        let type_checker = self
            .type_checker
            .get_or_insert_with(|| allocate_into_baseline(TypeChecker::default));
        match type_checker.run_with(&SourceFile::new(code, script_name), &context, state.config) {
            Ok(None) => PreFeed::Run,
            Ok(Some(diagnostics)) => PreFeed::Reject(event(pb::child_event::Kind::TypingError(pb::TypingError {
                diagnostics: diagnostics.to_string(),
            }))),
            Err(err) => PreFeed::Reject(protocol_violation(&format!("type checker failed: {err}"))),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            type_check: false,
            type_check_stubs: None,
            checking: TypeCheckingConfig::default(),
            options: CompileOptions {
                assert_message_annotations: AssertMessageAnnotations::default(),
                source_scan_threshold: SOURCE_SCAN_THRESHOLD,
            },
        }
    }
}

/// Maps the interpreter's progress onto the child's step: a completed feed
/// returns its session, every other progress is a suspension.
fn step(result: Result<ReplProgress, Box<ReplStartError>>) -> Step<PythonEngine> {
    match result {
        Ok(ReplProgress::Complete { repl, value }) => Ok(Outcome::Complete { repl, value }),
        Ok(progress) => Ok(Outcome::Suspended(progress)),
        Err(err) => Err(Box::new(Failed {
            repl: err.repl,
            error: err.error,
        })),
    }
}

/// Validates a dict of dotted module paths to `.pyi` source or `None`.
/// Paths must belong to a requested module; errors are protocol-violation messages.
fn stubs_from_answer(answer: &MontyObject, asked: &[String]) -> Result<Vec<ModuleStub>, String> {
    let answer = answer.as_ref();
    let pairs = match answer.pairs() {
        Some(pairs) if answer.type_name() == "dict" => pairs,
        _ => return Err("import_stubs must be answered with a dict of module to stub source".to_owned()),
    };
    let mut stubs = Vec::new();
    for (key, value) in pairs {
        let Some(module) = key.as_str() else {
            return Err("import_stubs answer keys must be module names".to_owned());
        };
        let top_level = module.split('.').next().unwrap_or_default();
        if !asked.iter().any(|name| name == top_level) {
            return Err(format!("import_stubs answered for {module:?}, which was not asked for"));
        }
        if let Some(source) = value.as_str() {
            stubs.push(ModuleStub::new(module, source).map_err(|err| format!("import_stubs: {err}"))?);
        } else if value.type_name() != "NoneType" {
            return Err(format!("import_stubs answer for {module:?} must be a str or None"));
        }
    }
    Ok(stubs)
}
