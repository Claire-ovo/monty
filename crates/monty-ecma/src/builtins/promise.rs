//! `Promise`, its combinators, `%AsyncFunction.prototype%`, and the global
//! scheduling functions (`queueMicrotask`, `setTimeout` and friends). The
//! settlement machinery and job queue live in `vm::promise`.

use super::{
    NativeFn, arg, install_constructor, install_function, install_hidden_constructor, install_symbol_function,
    install_tag, key, plain_object, raw_object, require_callable,
};
use crate::{
    error::{RunResult, Throw},
    heap::{FunctionObject, Handler, HeapId, PropFlags, PropertyKey, Target},
    intern::{StaticAtom, WellKnownSymbol},
    realm::ErrorKind,
    value::JsValue,
    vm::{Vm, number::to_uint32},
};

/// Which combinator an element function reports to (its `kind` argument).
const KIND_ALL: f64 = 0.0;
const KIND_SETTLED_FULFILLED: f64 = 1.0;
const KIND_SETTLED_REJECTED: f64 = 2.0;
const KIND_ANY: f64 = 3.0;

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    let prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.promise_prototype = prototype;
    for native in [
        NativeFn::PromiseProtoThen,
        NativeFn::PromiseProtoCatch,
        NativeFn::PromiseProtoFinally,
    ] {
        install_function(vm, prototype, native);
    }
    install_tag(vm, prototype, "Promise");
    let constructor = install_constructor(vm, global, NativeFn::Promise, prototype);
    for native in [
        NativeFn::PromiseResolve,
        NativeFn::PromiseReject,
        NativeFn::PromiseAll,
        NativeFn::PromiseAllSettled,
        NativeFn::PromiseRace,
        NativeFn::PromiseAny,
        NativeFn::PromiseWithResolvers,
        NativeFn::PromiseTry,
    ] {
        install_function(vm, constructor, native);
    }
    // `Promise[Symbol.species]` is not consulted, but the property exists for feature checks
    install_symbol_function(vm, constructor, WellKnownSymbol::Species, NativeFn::PromiseResolve);
    let function_prototype = vm.session.realm.function_prototype;
    let async_function_prototype = raw_object(vm, Some(function_prototype));
    vm.session.realm.async_function_prototype = async_function_prototype;
    install_tag(vm, async_function_prototype, "AsyncFunction");
    install_hidden_constructor(vm, NativeFn::AsyncFunction, async_function_prototype);
    for native in [
        NativeFn::QueueMicrotask,
        NativeFn::SetTimeout,
        NativeFn::SetInterval,
        NativeFn::SetImmediate,
        NativeFn::ClearTimeout,
        NativeFn::ClearInterval,
        NativeFn::ClearImmediate,
    ] {
        install_function(vm, global, native);
    }
}

impl Vm<'_> {
    /// `CreateResolvingFunctions`: a `resolve`/`reject` pair for `promise`,
    /// sharing a flag so only the first call counts.
    pub(crate) fn resolving_functions(&mut self, promise: HeapId) -> (JsValue, JsValue) {
        let flag = self.new_object_with_proto(None);
        let done = key(self, "done");
        self.define_data(flag.heap_id().expect("object"), done, JsValue::Bool(false));
        let resolve = self.settling_function(NativeFn::PromiseResolveFunction, promise, &flag);
        let reject = self.settling_function(NativeFn::PromiseRejectFunction, promise, &flag);
        flag.drop_with(&mut self.session.heap);
        (resolve, reject)
    }

    /// A native bound to `promise` as `this` with `flag` as its first argument.
    fn settling_function(&mut self, native: NativeFn, promise: HeapId, flag: &JsValue) -> JsValue {
        self.session.heap.inc_ref(promise);
        self.bound_native(
            native,
            JsValue::Ref(promise),
            vec![flag.clone_with_heap(&self.session.heap)],
        )
    }

    /// A native function with a fixed `this` and leading arguments; `this`
    /// and `args` are consumed.
    pub(super) fn bound_native(&mut self, native: NativeFn, this: JsValue, args: Vec<JsValue>) -> JsValue {
        let target = self.new_function_object(FunctionObject::Native(native), StaticAtom::Empty.id(), 1);
        let bound = FunctionObject::Bound {
            target,
            this,
            args: args.into_boxed_slice(),
        };
        JsValue::Ref(self.new_function_object(bound, StaticAtom::Empty.id(), 1))
    }

    /// A new promise already settled with `settled` (one owned reference).
    fn settled_promise(&mut self, value: JsValue, rejected: bool) -> RunResult<JsValue> {
        let promise = self.new_promise();
        if rejected {
            self.reject_promise(promise, value);
        } else {
            self.resolve_promise(promise, value)?;
        }
        Ok(JsValue::Ref(promise))
    }
}

/// The promise a method was called on, or V8's incompatible-receiver error.
fn receiver(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<HeapId> {
    if let JsValue::Ref(id) = this
        && vm.is_promise(*id)
    {
        return Ok(*id);
    }
    let shown = vm.inspect_short(this)?;
    Err(vm.type_error(&format!(
        "Method Promise.prototype.{method} called on incompatible receiver {shown}"
    )))
}

// === Constructor and resolving functions ===

/// `new Promise(executor)`.
pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    if !construct {
        return Err(vm.type_error("Promise constructor cannot be invoked without 'new'"));
    }
    let executor = arg(args, 0);
    if !vm.is_callable(executor) {
        let shown = vm.inspect_short(executor)?;
        return Err(vm.type_error(&format!("Promise resolver {shown} is not a function")));
    }
    let promise = vm.new_promise();
    let (resolve, reject) = vm.resolving_functions(promise);
    let reject_again = reject.clone_with_heap(&vm.session.heap);
    let outcome = vm.call(
        executor.clone_with_heap(&vm.session.heap),
        JsValue::Undefined,
        vec![resolve, reject],
    );
    match outcome {
        Ok(result) => {
            result.drop_with(&mut vm.session.heap);
            reject_again.drop_with(&mut vm.session.heap);
        }
        Err(Throw::Value(reason)) => {
            let result = vm.call(reject_again, JsValue::Undefined, vec![reason]);
            match result {
                Ok(result) => result.drop_with(&mut vm.session.heap),
                Err(err) => {
                    vm.session.heap.dec_ref(promise);
                    return Err(err);
                }
            }
        }
        Err(err) => {
            reject_again.drop_with(&mut vm.session.heap);
            vm.session.heap.dec_ref(promise);
            return Err(err);
        }
    }
    Ok(JsValue::Ref(promise))
}

/// The `resolve` half of a resolving pair: `this` is the promise, the first
/// argument the shared flag.
pub(super) fn resolve_function(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    settle_once(vm, this, args, false)
}

pub(super) fn reject_function(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    settle_once(vm, this, args, true)
}

fn settle_once(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], rejected: bool) -> RunResult<JsValue> {
    let JsValue::Ref(promise) = this else {
        return Ok(JsValue::Undefined);
    };
    let flag = arg(args, 0);
    let done_key = key(vm, "done");
    let done = vm.get(flag, done_key)?;
    if vm.to_boolean(&done) {
        return Ok(JsValue::Undefined);
    }
    vm.set(flag, done_key, JsValue::Bool(true))?;
    let value = arg(args, 1).clone_with_heap(&vm.session.heap);
    if rejected {
        vm.reject_promise(*promise, value);
    } else {
        vm.resolve_promise(*promise, value)?;
    }
    Ok(JsValue::Undefined)
}

// === Statics ===

pub(super) fn resolve(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = arg(args, 0).clone_with_heap(&vm.session.heap);
    let promise = vm.promise_resolve(value)?;
    Ok(JsValue::Ref(promise))
}

pub(super) fn reject(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let reason = arg(args, 0).clone_with_heap(&vm.session.heap);
    vm.settled_promise(reason, true)
}

pub(super) fn with_resolvers(vm: &mut Vm<'_>, _: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let promise = vm.new_promise();
    let (resolve, reject) = vm.resolving_functions(promise);
    let result = plain_object(vm);
    for (name, value) in [
        ("promise", JsValue::Ref(promise)),
        ("resolve", resolve),
        ("reject", reject),
    ] {
        let key = key(vm, name);
        vm.define_data(result, key, value);
    }
    Ok(JsValue::Ref(result))
}

/// `Promise.try(fn, ...args)`: the call's result or throw as a promise.
pub(super) fn try_(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let rest: Vec<JsValue> = args
        .iter()
        .skip(1)
        .map(|a| a.clone_with_heap(&vm.session.heap))
        .collect();
    match vm.call(callback.clone_with_heap(&vm.session.heap), JsValue::Undefined, rest) {
        Ok(value) => vm.settled_promise(value, false),
        Err(Throw::Value(reason)) => vm.settled_promise(reason, true),
        Err(err) => Err(err),
    }
}

/// The combinators' shared shape: a result promise and, per element, a
/// reaction on `PromiseResolve(element)`.
enum Combinator {
    All,
    AllSettled,
    Race,
    Any,
}

pub(super) fn all(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    combine(vm, arg(args, 0), &Combinator::All)
}

pub(super) fn all_settled(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    combine(vm, arg(args, 0), &Combinator::AllSettled)
}

pub(super) fn race(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    combine(vm, arg(args, 0), &Combinator::Race)
}

pub(super) fn any(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    combine(vm, arg(args, 0), &Combinator::Any)
}

fn combine(vm: &mut Vm<'_>, iterable: &JsValue, combinator: &Combinator) -> RunResult<JsValue> {
    let result = vm.new_promise();
    let items = match vm.iterate_to_vec(iterable.clone_with_heap(&vm.session.heap)) {
        Ok(items) => items,
        Err(Throw::Value(reason)) => {
            vm.reject_promise(result, reason);
            return Ok(JsValue::Ref(result));
        }
        Err(err) => {
            vm.session.heap.dec_ref(result);
            return Err(err);
        }
    };
    let (resolve, reject) = vm.resolving_functions(result);
    if matches!(combinator, Combinator::Race) {
        for item in items {
            let promise = vm.promise_resolve(item)?;
            vm.session.heap.inc_ref(result);
            vm.promise_then(promise, Handler::Identity, Handler::Thrower, Target::Promise(result));
            vm.session.heap.dec_ref(promise);
        }
        vm.drop_all(resolve, vec![reject]);
        return Ok(JsValue::Ref(result));
    }
    // the element functions share a state object: the values collected so far,
    // how many are outstanding, and the result promise to settle
    let count = items.len();
    let values = vm.new_array((0..count).map(|_| JsValue::Undefined).collect())?;
    let state = vm.new_object_with_proto(None);
    let state_id = state.heap_id().expect("object");
    vm.session.heap.inc_ref(result);
    for (name, value) in [
        ("values", values),
        ("remaining", JsValue::Number(count as f64)),
        ("promise", JsValue::Ref(result)),
    ] {
        let key = key(vm, name);
        vm.define_data(state_id, key, value);
    }
    if count == 0 {
        finish_combinator(vm, &state, combinator)?;
    }
    for (index, item) in items.into_iter().enumerate() {
        let promise = vm.promise_resolve(item)?;
        let element = |vm: &mut Vm<'_>, kind: f64| {
            vm.bound_native(
                NativeFn::PromiseCombinatorElement,
                state.clone_with_heap(&vm.session.heap),
                vec![JsValue::Number(index as f64), JsValue::Number(kind)],
            )
        };
        let (on_fulfilled, on_rejected) = match combinator {
            Combinator::All => (
                Handler::Call(element(vm, KIND_ALL)),
                Handler::Call(reject.clone_with_heap(&vm.session.heap)),
            ),
            Combinator::AllSettled => (
                Handler::Call(element(vm, KIND_SETTLED_FULFILLED)),
                Handler::Call(element(vm, KIND_SETTLED_REJECTED)),
            ),
            Combinator::Any => (
                Handler::Call(resolve.clone_with_heap(&vm.session.heap)),
                Handler::Call(element(vm, KIND_ANY)),
            ),
            Combinator::Race => unreachable!("handled above"),
        };
        vm.promise_then(promise, on_fulfilled, on_rejected, Target::None);
        vm.session.heap.dec_ref(promise);
    }
    vm.drop_all(resolve, vec![reject]);
    state.drop_with(&mut vm.session.heap);
    Ok(JsValue::Ref(result))
}

/// One element settling: records it and settles the result once all are in.
/// `this` is the state object, the arguments `index, kind, value`.
pub(super) fn combinator_element(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let index = vm.coerce_integer(arg(args, 0))?;
    let kind = vm.coerce_number(arg(args, 1))?;
    let value = arg(args, 2).clone_with_heap(&vm.session.heap);
    let stored = if kind == KIND_ALL || kind == KIND_ANY {
        value
    } else {
        let entry = plain_object(vm);
        let status = key(vm, "status");
        let (label, field) = if kind == KIND_SETTLED_FULFILLED {
            ("fulfilled", "value")
        } else {
            ("rejected", "reason")
        };
        let label = vm.atom_value(label);
        vm.define_data(entry, status, label);
        let field = key(vm, field);
        vm.define_data(entry, field, value);
        JsValue::Ref(entry)
    };
    let values_key = key(vm, "values");
    let values = vm.get(this, values_key)?;
    let index_key = vm.coerce_key(JsValue::Number(index))?;
    let set = vm.set(&values, index_key, stored);
    values.drop_with(&mut vm.session.heap);
    set?;
    let remaining_key = key(vm, "remaining");
    let remaining = vm.get(this, remaining_key)?;
    let left = vm.coerce_number(&remaining)? - 1.0;
    vm.set(this, remaining_key, JsValue::Number(left))?;
    if left <= 0.0 {
        let combinator = if kind == KIND_ANY {
            Combinator::Any
        } else {
            Combinator::All
        };
        finish_combinator(vm, this, &combinator)?;
    }
    Ok(JsValue::Undefined)
}

/// Settles a combinator's result promise from its state object: the values
/// array, or for `any` an `AggregateError` of the collected rejections.
fn finish_combinator(vm: &mut Vm<'_>, state: &JsValue, combinator: &Combinator) -> RunResult<()> {
    let values_key = key(vm, "values");
    let values = vm.get(state, values_key)?;
    let promise_key = key(vm, "promise");
    let promise = vm.get(state, promise_key)?;
    let Some(promise_id) = promise.heap_id() else {
        values.drop_with(&mut vm.session.heap);
        return Ok(());
    };
    let outcome = if matches!(combinator, Combinator::Any) {
        let error = vm.new_error(ErrorKind::AggregateError, "All promises were rejected");
        if let JsValue::Ref(error_id) = &error {
            vm.define_with_flags(
                *error_id,
                PropertyKey::Atom(StaticAtom::Errors.id()),
                values,
                PropFlags::HIDDEN,
            );
        }
        vm.reject_promise(promise_id, error);
        Ok(())
    } else {
        vm.resolve_promise(promise_id, values)
    };
    promise.drop_with(&mut vm.session.heap);
    outcome
}

// === Promise.prototype ===

pub(super) fn then(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let promise = receiver(vm, this, "then")?;
    let on_fulfilled = handler(vm, arg(args, 0), Handler::Identity);
    let on_rejected = handler(vm, arg(args, 1), Handler::Thrower);
    Ok(chain(vm, promise, on_fulfilled, on_rejected))
}

pub(super) fn catch(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let promise = receiver(vm, this, "catch")?;
    let on_rejected = handler(vm, arg(args, 0), Handler::Thrower);
    Ok(chain(vm, promise, Handler::Identity, on_rejected))
}

pub(super) fn finally(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let promise = receiver(vm, this, "finally")?;
    let callback = arg(args, 0);
    let (on_fulfilled, on_rejected) = if vm.is_callable(callback) {
        (
            Handler::Finally(callback.clone_with_heap(&vm.session.heap)),
            Handler::Finally(callback.clone_with_heap(&vm.session.heap)),
        )
    } else {
        (Handler::Identity, Handler::Thrower)
    };
    Ok(chain(vm, promise, on_fulfilled, on_rejected))
}

/// A `then` argument as a handler: called when callable, else the default pass-through.
fn handler(vm: &mut Vm<'_>, value: &JsValue, default: Handler) -> Handler {
    if vm.is_callable(value) {
        Handler::Call(value.clone_with_heap(&vm.session.heap))
    } else {
        default
    }
}

/// Attaches handlers feeding a new derived promise, which is returned.
fn chain(vm: &mut Vm<'_>, promise: HeapId, on_fulfilled: Handler, on_rejected: Handler) -> JsValue {
    let derived = vm.new_promise();
    vm.session.heap.inc_ref(derived);
    vm.promise_then(promise, on_fulfilled, on_rejected, Target::Promise(derived));
    JsValue::Ref(derived)
}

// === Scheduling globals ===

/// Node's `ERR_INVALID_ARG_TYPE` wording for a non-function callback.
fn callback_error(vm: &mut Vm<'_>, value: &JsValue) -> RunResult<Throw> {
    let received = match value {
        JsValue::Undefined | JsValue::Hole => "undefined".to_owned(),
        JsValue::Null => "null".to_owned(),
        JsValue::Bool(b) => format!("type boolean ({b})"),
        JsValue::Number(_) => format!("type number ({})", vm.inspect(value)?),
        JsValue::Atom(_) | JsValue::Str(_) => format!("type string ('{}')", vm.inspect(value)?),
        JsValue::Symbol(_) => format!("type symbol ({})", vm.inspect(value)?),
        JsValue::BigInt(_) => "type bigint".to_owned(),
        JsValue::Ref(_) => {
            let shown = vm.inspect_short(value)?;
            let name = shown.trim_start_matches("#<").trim_end_matches('>').to_owned();
            format!("an instance of {name}")
        }
    };
    Ok(vm.type_error(&format!(
        "The \"callback\" argument must be of type function. Received {received}"
    )))
}

pub(super) fn queue_microtask(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(callback_error(vm, callback)?);
    }
    vm.queue_callback(callback.clone_with_heap(&vm.session.heap));
    Ok(JsValue::Undefined)
}

/// `setTimeout(callback, delay, ...args)` / `setInterval`: the timer id.
fn schedule(vm: &mut Vm<'_>, args: &[JsValue], repeat: bool, immediate: bool) -> RunResult<JsValue> {
    let callback = arg(args, 0);
    if !vm.is_callable(callback) {
        return Err(callback_error(vm, callback)?);
    }
    let skip = if immediate { 1 } else { 2 };
    let delay = if immediate {
        0.0
    } else {
        match arg(args, 1) {
            JsValue::Undefined => 1.0,
            other => vm.coerce_number(other)?,
        }
    };
    let rest: Vec<JsValue> = args
        .iter()
        .skip(skip)
        .map(|a| a.clone_with_heap(&vm.session.heap))
        .collect();
    let id = vm.add_timer(
        callback.clone_with_heap(&vm.session.heap),
        rest,
        delay,
        repeat,
        immediate,
    );
    Ok(JsValue::Number(f64::from(id)))
}

pub(super) fn set_timeout(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    schedule(vm, args, false, false)
}

pub(super) fn set_interval(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    schedule(vm, args, true, false)
}

pub(super) fn set_immediate(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    schedule(vm, args, false, true)
}

pub(super) fn clear_timeout(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    if let JsValue::Number(id) = arg(args, 0) {
        vm.clear_timer(to_uint32(*id));
    }
    Ok(JsValue::Undefined)
}
