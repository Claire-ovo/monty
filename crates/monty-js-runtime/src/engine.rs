//! The JavaScript engine behind the protocol child.
//!
//! Feeds run to completion in this phase: host calls, name lookups, futures
//! and dumps are not reachable from JavaScript yet, so the suspension type
//! is uninhabited and the resume methods cannot be called.

use monty_ecma::{JsProgress, JsRepl};
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

/// A suspended JavaScript feed, which nothing produces yet.
pub(crate) enum Never {}

impl Engine for JsEngine {
    type Repl = JsRepl;
    type Progress = Never;
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
        match repl.feed(code, &inputs, print) {
            JsProgress::Complete { repl, value } => Ok(Outcome::Complete { repl, value }),
            JsProgress::Failed { repl, error } => Err(Box::new(Failed { repl, error })),
        }
    }

    fn feed_settled(&mut self, _: bool) {}

    fn resume_call(&mut self, progress: Never, _: ExtFunctionResult, _: PrintWriter<'_>) -> Step<Self> {
        match progress {}
    }

    fn resume_eager(
        &mut self,
        progress: Never,
        _: Result<MontyObject, MontyException>,
        _: PrintWriter<'_>,
    ) -> Step<Self> {
        match progress {}
    }

    fn resume_name_lookup(&mut self, progress: Never, _: NameLookupResult, _: PrintWriter<'_>) -> Step<Self> {
        match progress {}
    }

    fn resume_futures(&mut self, progress: Never, _: Vec<(u32, ExtFunctionResult)>, _: PrintWriter<'_>) -> Step<Self> {
        match progress {}
    }

    fn abort(&mut self, progress: Never, _: MontyException, _: PrintWriter<'_>) -> Step<Self> {
        match progress {}
    }

    fn suspension(progress: &mut Never) -> Suspension<'_> {
        match *progress {}
    }

    fn into_repl(progress: Never) -> JsRepl {
        match progress {}
    }

    fn repl_tracker(repl: &JsRepl) -> &ResourceTracker {
        repl.tracker()
    }

    fn progress_tracker(progress: &Never) -> &ResourceTracker {
        match *progress {}
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
