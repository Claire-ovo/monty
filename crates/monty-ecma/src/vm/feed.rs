//! The feed loop: runs the module body, then drains the job and timer
//! queues, each job starting a frame the dispatch loop runs (never a nested
//! Rust call), until everything is settled or the host must be asked.

use std::sync::Arc;

use super::{
    Completion, Delivery, Outcome, Vm,
    call::{Callable, Invocation},
    host::HostRequest,
};
use crate::{
    error::{RunResult, Throw},
    function::Function,
    heap::Settled,
    realm::ErrorKind,
    value::JsValue,
};

/// How a feed left the interpreter.
pub(crate) enum FeedEnd {
    /// Every frame, job and timer finished: the feed's value.
    Complete(JsValue),
    /// The host must answer before the feed can continue.
    Suspended(HostRequest),
}

impl Vm<'_> {
    /// Runs a module body and everything it schedules.
    pub(crate) fn start_feed(&mut self, module: Arc<Function>) -> RunResult<FeedEnd> {
        self.traceback.clear();
        let is_async = module.kind.is_async();
        self.module_async = is_async;
        self.push_frame(module, None, Vec::new(), Invocation::call(JsValue::Undefined))?;
        if is_async {
            self.start_coroutine();
        }
        self.continue_feed()
    }

    /// Runs whatever frames are on the stack, then the queues, until the
    /// feed completes or suspends.
    pub(crate) fn continue_feed(&mut self) -> RunResult<FeedEnd> {
        let mut jobs_run = 0usize;
        loop {
            if !self.frames.is_empty() {
                match self.run(0)? {
                    Outcome::Return(value) => self.keep_feed_result(value),
                    Outcome::Done => {}
                    Outcome::Suspend(request) => return Ok(FeedEnd::Suspended(request)),
                    Outcome::Yield(value) => {
                        value.drop_with(&mut self.session.heap);
                        return Err(self.internal_error("the module frame yielded"));
                    }
                }
                continue;
            }
            if let Some(job) = self.jobs.pop_front() {
                self.schedule_job(job)?;
                jobs_run += 1;
                self.tracker().check_memory_time_every(jobs_run)?;
            } else if let Some(timer) = self.pop_timer() {
                self.schedule_timer(timer)?;
            } else {
                break;
            }
        }
        if !self.host_futures.is_empty() {
            return Ok(FeedEnd::Suspended(self.resolve_futures_request()));
        }
        let value = self.feed_result.take().unwrap_or(JsValue::Undefined);
        self.finish_feed(value).map(FeedEnd::Complete)
    }

    /// The module frame's completion value; later top-level deliveries
    /// (none are expected) are dropped.
    pub(super) fn keep_feed_result(&mut self, value: JsValue) {
        if self.feed_result.is_none() {
            self.feed_result = Some(value);
        } else {
            value.drop_with(&mut self.session.heap);
        }
    }

    /// The feed's value once every queue is empty: an async body's promise is
    /// read, and a rejection nobody handled fails the feed as an uncaught error.
    fn finish_feed(&mut self, value: JsValue) -> RunResult<JsValue> {
        let value = match value {
            JsValue::Ref(id) if self.module_async && self.is_promise(id) => {
                let settled = self.promise_settlement(id);
                value.drop_with(&mut self.session.heap);
                match settled {
                    Some(Settled::Fulfilled(value)) => value,
                    Some(Settled::Rejected(reason)) => return Err(Throw::Value(reason)),
                    None => return Err(self.throw_error(ErrorKind::Error, "unsettled top-level await")),
                }
            }
            other => other,
        };
        match self.take_unhandled_rejection() {
            Some(reason) => {
                value.drop_with(&mut self.session.heap);
                Err(Throw::Value(reason))
            }
            None => Ok(value),
        }
    }

    /// Starts a callback from the job queue so the dispatch loop runs it with
    /// `completion` as its continuation. A native (or anything that cannot
    /// get a frame) runs now and its outcome goes straight to the completion.
    pub(super) fn invoke(
        &mut self,
        callee: JsValue,
        this: JsValue,
        args: Vec<JsValue>,
        completion: Completion,
    ) -> RunResult<()> {
        match self.callable(&callee) {
            Some(Callable::Closure { function, cells }) if !function.kind.is_class_constructor() => {
                let is_async = function.kind.is_async();
                self.push_closure_frame(function, &cells, callee, args, Invocation::call(this))?;
                self.frames.last_mut().expect("frame").completion = completion;
                if is_async {
                    self.start_coroutine();
                }
                Ok(())
            }
            Some(Callable::Bound {
                target,
                this: bound_this,
                args: bound_args,
            }) => {
                self.session.heap.inc_ref(target);
                self.drop_all(this, vec![callee]);
                let mut all = bound_args;
                all.extend(args);
                self.invoke(JsValue::Ref(target), bound_this, all, completion)
            }
            _ => {
                let result = self.call(callee, this, args);
                let outcome = match self.native_value(result) {
                    Ok(value) => Settled::Fulfilled(value),
                    Err(Throw::Value(reason)) => Settled::Rejected(reason),
                    Err(err) => {
                        self.discard_completion(completion);
                        return Err(err);
                    }
                };
                match self.complete(completion, outcome)? {
                    Delivery::Propagate(reason) => Err(Throw::Value(reason)),
                    Delivery::Value(value) | Delivery::Yielded(value) => {
                        value.drop_with(&mut self.session.heap);
                        Ok(())
                    }
                    Delivery::Nothing => Ok(()),
                }
            }
        }
    }
}
