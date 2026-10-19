//! Promises and the job queue: settling, reactions, `await`, microtasks and
//! the timer queue. Jobs run only when the stack is empty, which the feed
//! lifecycle guarantees by draining them after the module body and after
//! every timer callback.

use std::mem;

use serde::{Deserialize, Serialize};

use super::{Completion, Vm, generator::ResumeMode};
use crate::{
    error::{RunResult, Throw},
    heap::{Handler, HeapId, JsHeapData, JsObject, ObjectClass, PromiseData, PromiseState, Reaction, Settled, Target},
    intern::StaticAtom,
    realm::ErrorKind,
    value::JsValue,
};

/// One microtask.
#[derive(Serialize, Deserialize)]
pub(crate) enum Job {
    /// A settled promise's reaction, with the settlement it reacts to.
    Reaction { reaction: Reaction, settled: Settled },
    /// `resolve(thenable)`: call `then` with the promise's resolving functions.
    Thenable {
        promise: HeapId,
        thenable: JsValue,
        then: JsValue,
    },
    /// `queueMicrotask(callback)`.
    Callback(JsValue),
}

/// A `setTimeout` / `setInterval` entry (its values are owned).
#[derive(Serialize, Deserialize)]
pub(crate) struct Timer {
    pub id: u32,
    /// When it fires on the virtual clock, in milliseconds.
    pub due: f64,
    /// Creation order, so equal due times fire in order.
    pub seq: u32,
    pub callback: JsValue,
    pub args: Vec<JsValue>,
    /// `setInterval`'s period: the timer re-arms after firing.
    pub interval: Option<f64>,
}

impl Vm<'_> {
    // === Creation and settlement ===

    /// A new pending promise with `Promise.prototype` (one owned reference).
    pub(crate) fn new_promise(&mut self) -> HeapId {
        let proto = self.session.realm.promise_prototype;
        self.session.heap.inc_ref(proto);
        self.session.heap.allocate(JsHeapData::Object(JsObject::with_class(
            Some(proto),
            ObjectClass::Promise(Box::new(PromiseData {
                state: PromiseState::Pending(Vec::new()),
                handled: false,
            })),
        )))
    }

    /// Whether `id` is a promise.
    pub(crate) fn is_promise(&self, id: HeapId) -> bool {
        matches!(self.object(id).map(|o| &o.class), Some(ObjectClass::Promise(_)))
    }

    /// `PromiseResolve(value)`: the value itself when it is a promise, else a
    /// new promise resolved with it. Consumes `value`; returns an owned reference.
    pub(crate) fn promise_resolve(&mut self, value: JsValue) -> RunResult<HeapId> {
        if let JsValue::Ref(id) = &value
            && self.is_promise(*id)
        {
            return Ok(*id);
        }
        let promise = self.new_promise();
        self.resolve_promise(promise, value)?;
        Ok(promise)
    }

    /// Resolves `promise` with `value` (consumed): a thenable is followed
    /// through a job, anything else fulfills directly.
    pub(crate) fn resolve_promise(&mut self, promise: HeapId, value: JsValue) -> RunResult<()> {
        if let JsValue::Ref(id) = &value {
            if *id == promise {
                value.drop_with(&mut self.session.heap);
                let error = self.new_error(ErrorKind::TypeError, "Chaining cycle detected for promise #<Promise>");
                self.reject_promise(promise, error);
                return Ok(());
            }
            let then = match self.get_atom(&value, StaticAtom::Then.id()) {
                Ok(then) => then,
                Err(Throw::Value(reason)) => {
                    value.drop_with(&mut self.session.heap);
                    self.reject_promise(promise, reason);
                    return Ok(());
                }
                Err(err) => {
                    value.drop_with(&mut self.session.heap);
                    return Err(err);
                }
            };
            if self.is_callable(&then) {
                self.session.heap.inc_ref(promise);
                self.jobs.push_back(Job::Thenable {
                    promise,
                    thenable: value,
                    then,
                });
                return Ok(());
            }
            then.drop_with(&mut self.session.heap);
        }
        self.settle(promise, Settled::Fulfilled(value));
        Ok(())
    }

    /// Rejects `promise` with `reason` (consumed).
    pub(crate) fn reject_promise(&mut self, promise: HeapId, reason: JsValue) {
        self.settle(promise, Settled::Rejected(reason));
    }

    /// Records a settlement and queues the reactions waiting for it; a
    /// settlement of an already settled promise is ignored.
    fn settle(&mut self, promise: HeapId, settled: Settled) {
        let stored = match &settled {
            Settled::Fulfilled(value) => PromiseState::Fulfilled(value.clone_with_heap(&self.session.heap)),
            Settled::Rejected(value) => PromiseState::Rejected(value.clone_with_heap(&self.session.heap)),
        };
        let Some(ObjectClass::Promise(data)) = self.object_mut(promise).map(|o| &mut o.class) else {
            settled.into_value().drop_with(&mut self.session.heap);
            stored_drop(self, stored);
            return;
        };
        let PromiseState::Pending(reactions) = &mut data.state else {
            settled.into_value().drop_with(&mut self.session.heap);
            stored_drop(self, stored);
            return;
        };
        let reactions = mem::take(reactions);
        let unhandled = reactions.is_empty() && matches!(settled, Settled::Rejected(_));
        data.state = stored;
        if unhandled {
            self.session.heap.inc_ref(promise);
            self.unhandled.push(promise);
        }
        for reaction in reactions {
            let settled = match &settled {
                Settled::Fulfilled(value) => Settled::Fulfilled(value.clone_with_heap(&self.session.heap)),
                Settled::Rejected(value) => Settled::Rejected(value.clone_with_heap(&self.session.heap)),
            };
            self.jobs.push_back(Job::Reaction { reaction, settled });
        }
        settled.into_value().drop_with(&mut self.session.heap);
    }

    /// `PerformPromiseThen`: attaches a reaction, running it at once (as a
    /// job) when the promise has already settled. The target's reference is
    /// consumed.
    pub(crate) fn promise_then(
        &mut self,
        promise: HeapId,
        on_fulfilled: Handler,
        on_rejected: Handler,
        target: Target,
    ) {
        let reaction = Reaction {
            target,
            on_fulfilled,
            on_rejected,
        };
        let Some(ObjectClass::Promise(data)) = self.object(promise).map(|o| &o.class) else {
            reaction_drop(self, reaction);
            return;
        };
        let snapshot = match &data.state {
            PromiseState::Pending(_) => None,
            PromiseState::Fulfilled(value) => Some(Settled::Fulfilled(value.clone_with_heap(&self.session.heap))),
            PromiseState::Rejected(value) => Some(Settled::Rejected(value.clone_with_heap(&self.session.heap))),
        };
        let Some(ObjectClass::Promise(data)) = self.object_mut(promise).map(|o| &mut o.class) else {
            unreachable!("checked above");
        };
        match snapshot {
            None => {
                if let PromiseState::Pending(reactions) = &mut data.state {
                    reactions.push(reaction);
                }
            }
            Some(settled) => {
                if matches!(settled, Settled::Rejected(_)) {
                    data.handled = true;
                }
                self.jobs.push_back(Job::Reaction { reaction, settled });
            }
        }
    }

    /// `await value` inside the coroutine `owner`: the frame resumes with
    /// the settlement of `PromiseResolve(value)`.
    pub(super) fn await_value(&mut self, owner: HeapId, value: JsValue) -> RunResult<()> {
        let promise = self.promise_resolve(value)?;
        self.session.heap.inc_ref(owner);
        self.promise_then(promise, Handler::Identity, Handler::Thrower, Target::Coroutine(owner));
        self.session.heap.dec_ref(promise);
        Ok(())
    }

    // === The job queue ===

    /// Queues a `queueMicrotask` callback (consumed).
    pub(crate) fn queue_callback(&mut self, callback: JsValue) {
        self.jobs.push_back(Job::Callback(callback));
    }

    /// Starts one job: a reaction, a thenable adoption or a callback. Work
    /// that needs JavaScript gets a frame with the right continuation; the
    /// rest settles immediately.
    pub(super) fn schedule_job(&mut self, job: Job) -> RunResult<()> {
        match job {
            Job::Callback(callback) => self.invoke(callback, JsValue::Undefined, Vec::new(), Completion::Discard),
            Job::Thenable {
                promise,
                thenable,
                then,
            } => {
                let (resolve, reject) = self.resolving_functions(promise);
                self.session.heap.dec_ref(promise);
                let reject_again = reject.clone_with_heap(&self.session.heap);
                self.invoke(
                    then,
                    thenable,
                    vec![resolve, reject],
                    Completion::Thenable { reject: reject_again },
                )
            }
            Job::Reaction { reaction, settled } => self.schedule_reaction(reaction, settled),
        }
    }

    /// Applies a reaction's handler for the settlement.
    fn schedule_reaction(&mut self, reaction: Reaction, settled: Settled) -> RunResult<()> {
        let Reaction {
            target,
            on_fulfilled,
            on_rejected,
        } = reaction;
        let (handler, other) = match settled {
            Settled::Fulfilled(_) => (on_fulfilled, on_rejected),
            Settled::Rejected(_) => (on_rejected, on_fulfilled),
        };
        handler_drop(self, other);
        match handler {
            Handler::Identity => self.settle_target(target, Settled::Fulfilled(settled.into_value())),
            Handler::Thrower => self.settle_target(target, Settled::Rejected(settled.into_value())),
            Handler::Settle(stored) => {
                settled.into_value().drop_with(&mut self.session.heap);
                self.settle_target(target, stored)
            }
            Handler::Call(function) => self.invoke(
                function,
                JsValue::Undefined,
                vec![settled.into_value()],
                Completion::Settle(target),
            ),
            Handler::Finally(function) => self.invoke(
                function,
                JsValue::Undefined,
                Vec::new(),
                Completion::Finally { target, settled },
            ),
        }
    }

    /// A `finally` callback returned `result`: the original settlement passes
    /// through to `target` once `result` settles (the spec's
    /// thenFinally/catchFinally, with the same number of microtask hops as Node).
    pub(super) fn finish_finally(&mut self, target: Target, settled: Settled, result: JsValue) -> RunResult<()> {
        let waited = self.promise_resolve(result)?;
        let relay = self.new_promise();
        self.session.heap.inc_ref(relay);
        self.promise_then(
            waited,
            Handler::Settle(settled),
            Handler::Thrower,
            Target::Promise(relay),
        );
        self.session.heap.dec_ref(waited);
        self.settle_target(target, Settled::Fulfilled(JsValue::Ref(relay)))
    }

    /// Delivers a settlement: resolves or rejects a promise, or puts an
    /// awaiting async function's frame back on the stack. Consumes `target`.
    pub(super) fn settle_target(&mut self, target: Target, outcome: Settled) -> RunResult<()> {
        match target {
            Target::None => {
                outcome.into_value().drop_with(&mut self.session.heap);
                Ok(())
            }
            Target::Promise(promise) => {
                let result = match outcome {
                    Settled::Fulfilled(value) => self.resolve_promise(promise, value),
                    Settled::Rejected(reason) => {
                        self.reject_promise(promise, reason);
                        Ok(())
                    }
                };
                self.session.heap.dec_ref(promise);
                result
            }
            Target::Coroutine(coroutine) => {
                let (mode, value) = match outcome {
                    Settled::Fulfilled(value) => (ResumeMode::Next, value),
                    Settled::Rejected(reason) => (ResumeMode::Throw, reason),
                };
                let result = self.resume_coroutine(coroutine, mode, value);
                self.session.heap.dec_ref(coroutine);
                result
            }
        }
    }

    /// Releases a reaction target that will not be settled.
    pub(super) fn drop_target(&mut self, target: Target) {
        target_drop(self, target);
    }

    /// Fires a timer: its callback gets a frame; an interval re-arms first.
    pub(super) fn schedule_timer(&mut self, timer: Timer) -> RunResult<()> {
        let callback = timer.callback.clone_with_heap(&self.session.heap);
        let args: Vec<JsValue> = timer
            .args
            .iter()
            .map(|arg| arg.clone_with_heap(&self.session.heap))
            .collect();
        if let Some(interval) = timer.interval {
            self.timers.push(Timer {
                due: self.timer_clock + interval,
                seq: self.timer_seq,
                ..timer
            });
            self.timer_seq += 1;
        } else {
            self.drop_all(timer.callback, timer.args);
        }
        self.invoke(callback, JsValue::Undefined, args, Completion::Discard)
    }

    /// Releases every queued job and timer (a feed that failed mid-way).
    pub(super) fn discard_jobs(&mut self) {
        for job in mem::take(&mut self.jobs) {
            match job {
                Job::Callback(callback) => callback.drop_with(&mut self.session.heap),
                Job::Thenable {
                    promise,
                    thenable,
                    then,
                } => {
                    self.session.heap.dec_ref(promise);
                    self.drop_all(thenable, vec![then]);
                }
                Job::Reaction { reaction, settled } => {
                    reaction_drop(self, reaction);
                    settled.into_value().drop_with(&mut self.session.heap);
                }
            }
        }
        for timer in mem::take(&mut self.timers) {
            self.drop_all(timer.callback, timer.args);
        }
        for promise in mem::take(&mut self.unhandled) {
            self.session.heap.dec_ref(promise);
        }
    }

    /// The reason of the first rejected promise that never got a handler,
    /// which fails the feed as an uncaught error. Clears the record.
    pub(crate) fn take_unhandled_rejection(&mut self) -> Option<JsValue> {
        let promises = mem::take(&mut self.unhandled);
        let mut reason = None;
        for promise in promises {
            if reason.is_none()
                && let Some(ObjectClass::Promise(data)) = self.object(promise).map(|o| &o.class)
                && !data.handled
                && let PromiseState::Rejected(value) = &data.state
            {
                reason = Some(value.clone_with_heap(&self.session.heap));
            }
            self.session.heap.dec_ref(promise);
        }
        reason
    }

    /// A settled promise's outcome (cloned), `None` while pending; reading a
    /// rejection counts as handling it.
    pub(crate) fn promise_settlement(&mut self, promise: HeapId) -> Option<Settled> {
        let settled = match self.object(promise).map(|o| &o.class) {
            Some(ObjectClass::Promise(data)) => match &data.state {
                PromiseState::Pending(_) => None,
                PromiseState::Fulfilled(value) => Some(Settled::Fulfilled(value.clone_with_heap(&self.session.heap))),
                PromiseState::Rejected(value) => Some(Settled::Rejected(value.clone_with_heap(&self.session.heap))),
            },
            _ => None,
        };
        if matches!(settled, Some(Settled::Rejected(_)))
            && let Some(ObjectClass::Promise(data)) = self.object_mut(promise).map(|o| &mut o.class)
        {
            data.handled = true;
        }
        settled
    }

    // === Timers ===

    /// Schedules a callback `delay` milliseconds of virtual time from now
    /// (at least one, as Node clamps; `immediate` fires before any delay);
    /// returns the id `clearTimeout` takes.
    pub(crate) fn add_timer(
        &mut self,
        callback: JsValue,
        args: Vec<JsValue>,
        delay: f64,
        repeat: bool,
        immediate: bool,
    ) -> u32 {
        let id = self.timer_seq;
        self.timer_seq += 1;
        let delay = if immediate {
            0.0
        } else if delay.is_finite() && delay >= 1.0 {
            delay
        } else {
            1.0
        };
        self.timers.push(Timer {
            id,
            due: self.timer_clock + delay,
            seq: id,
            callback,
            args,
            interval: repeat.then_some(delay),
        });
        id
    }

    /// Cancels a timer; unknown ids are ignored.
    pub(crate) fn clear_timer(&mut self, id: u32) {
        if let Some(index) = self.timers.iter().position(|timer| timer.id == id) {
            let timer = self.timers.swap_remove(index);
            self.drop_all(timer.callback, timer.args);
        }
    }

    /// The next timer to fire, advancing the virtual clock to it.
    pub(super) fn pop_timer(&mut self) -> Option<Timer> {
        let index = (0..self.timers.len()).min_by(|a, b| {
            let (a, b) = (&self.timers[*a], &self.timers[*b]);
            a.due.total_cmp(&b.due).then(a.seq.cmp(&b.seq))
        })?;
        let timer = self.timers.swap_remove(index);
        self.timer_clock = self.timer_clock.max(timer.due);
        Some(timer)
    }
}

fn stored_drop(vm: &mut Vm<'_>, state: PromiseState) {
    match state {
        PromiseState::Pending(reactions) => {
            for reaction in reactions {
                reaction_drop(vm, reaction);
            }
        }
        PromiseState::Fulfilled(value) | PromiseState::Rejected(value) => value.drop_with(&mut vm.session.heap),
    }
}

fn handler_drop(vm: &mut Vm<'_>, handler: Handler) {
    match handler {
        Handler::Identity | Handler::Thrower => {}
        Handler::Call(function) | Handler::Finally(function) => function.drop_with(&mut vm.session.heap),
        Handler::Settle(settled) => settled.into_value().drop_with(&mut vm.session.heap),
    }
}

fn target_drop(vm: &mut Vm<'_>, target: Target) {
    match target {
        Target::Promise(id) | Target::Coroutine(id) => vm.session.heap.dec_ref(id),
        Target::None => {}
    }
}

fn reaction_drop(vm: &mut Vm<'_>, reaction: Reaction) {
    target_drop(vm, reaction.target);
    handler_drop(vm, reaction.on_fulfilled);
    handler_drop(vm, reaction.on_rejected);
}
