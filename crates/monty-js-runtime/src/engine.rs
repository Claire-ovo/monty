//! The JavaScript engine behind the protocol child.
//!
//! A feed runs until it completes, fails or suspends on the host: a call of
//! an undeclared name (`FunctionCall`), a read of one (`NameLookup`) or
//! promises waiting on host futures (`ResolveFutures`). Idle and suspended
//! sessions dump and load through `monty_ecma::dump`.

use monty_ecma::{JsDump, JsProgress, JsRepl, JsSessionRef, JsSuspension, JsSuspensionKind, dump};
use monty_proto::{
    pb,
    worker::{
        Engine, Failed, Loaded, LoadedSession, Outcome, PreFeed, SessionRef, Step, Suspension, error_event_from,
        protocol_violation,
    },
};
use monty_types::{
    ExcType, ExtFunctionResult, Language, MontyException, MontyObject, NameLookupResult, NamedValues, OsPolicy,
    PrintWriter, ResourceLimits, ResourceTracker,
};

/// The engine state the child drives: nothing beyond the session yet.
#[derive(Default)]
pub(crate) struct JsEngine;

impl Engine for JsEngine {
    type Repl = JsRepl;
    type Progress = JsSuspension;
    type Restored = ();

    const LANGUAGES: &'static [Language] = &[Language::JavaScript, Language::TypeScript];

    // TODO(ts-typecheck): accept `type_check` once a tsgo-backed checker exists.
    fn configure(&mut self, configure: &pb::Configure) -> Result<(), Box<pb::ChildEvent>> {
        if configure.type_check {
            let exc = MontyException::new(
                ExcType::RuntimeError,
                Some("type checking is not supported for JavaScript yet".to_owned()),
            );
            return Err(Box::new(error_event_from(&exc)));
        }
        Ok(())
    }

    fn type_checks(&self) -> bool {
        false
    }

    fn create_repl(
        &mut self,
        script_name: &str,
        limits: ResourceLimits,
        os_policy: OsPolicy,
    ) -> Result<JsRepl, Box<pb::ChildEvent>> {
        // the intrinsics need a few hundred heap entries, so only a memory limit
        // too small to run anything fails here; the host learns why
        JsRepl::new(script_name, limits)
            .map(|repl| repl.with_os_policy(os_policy))
            .map_err(|err| {
                let exc = MontyException::new(ExcType::MemoryError, Some(err.to_string()));
                Box::new(error_event_from(&exc))
            })
    }

    fn check_source(repl: &JsRepl, code: &str) -> Result<(), MontyException> {
        repl.check_source(code)
    }

    fn set_cwd(_: &mut JsRepl, _: &str) {}

    fn pre_feed(&mut self, _: &JsRepl, _: &str, _: &str, _: bool) -> PreFeed {
        PreFeed::Run
    }

    fn resume_pre_feed(&mut self, _: &JsRepl, _: &str, _: &str, _: Option<ExtFunctionResult>) -> PreFeed {
        PreFeed::Run
    }

    fn feed_start(&mut self, repl: JsRepl, code: &str, inputs: NamedValues, print: PrintWriter<'_>) -> Step<Self> {
        step(repl.feed(code, &inputs, print))
    }

    fn feed_settled(&mut self, _: bool) {}

    fn resume_call(&mut self, progress: JsSuspension, result: ExtFunctionResult, print: PrintWriter<'_>) -> Step<Self> {
        step(progress.resume_call(result, print))
    }

    fn resume_eager(
        &mut self,
        progress: JsSuspension,
        result: Result<MontyObject, MontyException>,
        print: PrintWriter<'_>,
    ) -> Step<Self> {
        // JavaScript never allows an eager await; treat the settled coroutine as a plain answer
        let result = match result {
            Ok(value) => ExtFunctionResult::Return(value),
            Err(exc) => ExtFunctionResult::Error(exc),
        };
        step(progress.resume_call(result, print))
    }

    fn resume_name_lookup(
        &mut self,
        progress: JsSuspension,
        result: NameLookupResult,
        print: PrintWriter<'_>,
    ) -> Step<Self> {
        step(progress.resume_name_lookup(result, print))
    }

    fn resume_futures(
        &mut self,
        progress: JsSuspension,
        results: Vec<(u32, ExtFunctionResult)>,
        print: PrintWriter<'_>,
    ) -> Step<Self> {
        step(progress.resume_futures(results, print))
    }

    fn abort(&mut self, progress: JsSuspension, exc: MontyException, print: PrintWriter<'_>) -> Step<Self> {
        step(progress.abort(&exc, print))
    }

    fn suspension(progress: &mut JsSuspension) -> Suspension<'_> {
        match progress.kind() {
            JsSuspensionKind::FunctionCall {
                function_name,
                args,
                call_id,
                position,
            } => Suspension::FunctionCall {
                function_name,
                args,
                call_id,
                object_id: None,
                allow_eager_await: false,
                position,
            },
            JsSuspensionKind::NameLookup { name, position } => Suspension::NameLookup {
                name,
                object_id: None,
                position,
            },
            JsSuspensionKind::ResolveFutures {
                pending_call_ids,
                position,
            } => Suspension::ResolveFutures {
                pending_call_ids,
                position,
            },
            JsSuspensionKind::OsCall {
                call,
                call_id,
                position,
            } => Suspension::OsCall {
                call,
                call_id,
                allow_eager_await: false,
                position,
            },
        }
    }

    fn into_repl(progress: JsSuspension) -> JsRepl {
        progress.into_repl()
    }

    fn repl_tracker(repl: &JsRepl) -> &ResourceTracker {
        repl.tracker()
    }

    fn progress_tracker(progress: &JsSuspension) -> &ResourceTracker {
        progress.tracker()
    }

    fn dump(&self, _: &str, session: SessionRef<'_, Self>) -> Result<Vec<u8>, String> {
        // the script name travels inside the session
        dump(match session {
            SessionRef::Idle(repl) => JsSessionRef::Idle(repl),
            SessionRef::Suspended(progress) => JsSessionRef::Suspended(progress),
        })
    }

    fn load(&self, state: &[u8], limits: &ResourceLimits) -> Result<Loaded<Self>, Box<pb::ChildEvent>> {
        let loaded = JsDump::load(state)
            .map_err(|err| Box::new(protocol_violation(&format!("failed to load session: {err}"))))?;
        // keep the snapshot's clocks and the stricter of its and this worker's limits
        let (script_name, session) = match loaded {
            JsDump::Idle(mut repl) => {
                repl.tracker_mut().tighten_limits(limits);
                (repl.script_name().to_owned(), LoadedSession::Idle(*repl))
            }
            JsDump::Suspended(mut suspension) => {
                suspension.tracker_mut().tighten_limits(limits);
                (
                    suspension.script_name().to_owned(),
                    LoadedSession::Suspended(*suspension),
                )
            }
        };
        Ok(Loaded {
            script_name,
            session,
            restored: (),
        })
    }

    fn adopt(&mut self, (): ()) {}

    fn reset(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Maps the engine's progress onto the child's step.
fn step(progress: JsProgress) -> Step<JsEngine> {
    match progress {
        JsProgress::Complete { repl, value } => Ok(Outcome::Complete { repl, value }),
        JsProgress::Failed { repl, error } => Err(Box::new(Failed { repl, error })),
        JsProgress::Suspended(suspension) => Ok(Outcome::Suspended(*suspension)),
    }
}
