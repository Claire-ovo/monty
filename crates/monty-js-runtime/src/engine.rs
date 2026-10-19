//! The JavaScript engine behind the protocol child.
//!
//! A feed runs until it completes, fails or suspends on the host: a call of
//! an undeclared name (`FunctionCall`), a read of one (`NameLookup`) or
//! promises waiting on host futures (`ResolveFutures`). Dumps are not
//! supported yet.

use monty_ecma::{JsProgress, JsRepl, JsSuspension, JsSuspensionKind};
use monty_proto::{
    pb,
    worker::{
        Engine, Failed, Loaded, Outcome, PreFeed, SessionRef, Step, Suspension, error_event_from, protocol_violation,
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

    // TODO(ts-syntax): add `Language::TypeScript` once the engine accepts it.
    const LANGUAGES: &'static [Language] = &[Language::JavaScript];

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

    fn create_repl(&mut self, script_name: &str, limits: ResourceLimits, os_policy: OsPolicy) -> JsRepl {
        // the intrinsics cannot exceed a sane limit; a failure here means the
        // host configured one too small to run anything
        JsRepl::new(script_name, limits)
            .unwrap_or_else(|err| panic!("cannot create the JavaScript session: {err}"))
            .with_os_policy(os_policy)
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

    fn dump(&self, _: &str, _: SessionRef<'_, Self>) -> Result<Vec<u8>, String> {
        Err("dumps are not supported for JavaScript yet".to_owned())
    }

    fn load(&self, _: &[u8], _: &ResourceLimits) -> Result<Loaded<Self>, Box<pb::ChildEvent>> {
        Err(Box::new(protocol_violation(
            "dumps are not supported for JavaScript yet",
        )))
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
