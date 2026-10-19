//! `%GeneratorFunction.prototype%` and `%GeneratorPrototype%`: the objects
//! every `function*` and every generator inherit from, with `next`, `return`
//! and `throw` driving the frame parked in the generator (see `vm::generator`).

use super::{NativeFn, arg, install_function, install_hidden_constructor, install_tag, raw_object};
use crate::{
    error::RunResult,
    heap::{HeapId, ObjectClass, PropFlags, PropertyKey},
    intern::StaticAtom,
    value::JsValue,
    vm::{Vm, generator::ResumeMode},
};

pub(super) fn install(vm: &mut Vm<'_>, _global: HeapId) {
    let iterator_prototype = vm.session.realm.iterator_prototype;
    let prototype = raw_object(vm, Some(iterator_prototype));
    vm.session.realm.generator_prototype = prototype;
    for native in [
        NativeFn::GeneratorProtoNext,
        NativeFn::GeneratorProtoReturn,
        NativeFn::GeneratorProtoThrow,
    ] {
        install_function(vm, prototype, native);
    }
    install_tag(vm, prototype, "Generator");
    let function_prototype = vm.session.realm.function_prototype;
    let generator_function_prototype = raw_object(vm, Some(function_prototype));
    vm.session.realm.generator_function_prototype = generator_function_prototype;
    vm.session.heap.inc_ref(prototype);
    vm.define_with_flags(
        generator_function_prototype,
        PropertyKey::Atom(StaticAtom::Prototype.id()),
        JsValue::Ref(prototype),
        PropFlags::CONFIGURABLE,
    );
    vm.session.heap.inc_ref(generator_function_prototype);
    vm.define_with_flags(
        prototype,
        PropertyKey::Atom(StaticAtom::Constructor.id()),
        JsValue::Ref(generator_function_prototype),
        PropFlags::CONFIGURABLE,
    );
    install_tag(vm, generator_function_prototype, "GeneratorFunction");
    install_hidden_constructor(vm, NativeFn::GeneratorFunction, generator_function_prototype);
}

/// The generator a method was called on, or V8's incompatible-receiver error.
fn receiver(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<HeapId> {
    if let JsValue::Ref(id) = this
        && matches!(vm.object(*id).map(|o| &o.class), Some(ObjectClass::Generator(_)))
    {
        return Ok(*id);
    }
    let shown = vm.inspect_short(this)?;
    Err(vm.type_error(&format!(
        "Method [Generator].prototype.{method} called on incompatible receiver {shown}"
    )))
}

fn resume(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], method: &str, mode: ResumeMode) -> RunResult<JsValue> {
    let id = receiver(vm, this, method)?;
    let value = arg(args, 0).clone_with_heap(&vm.session.heap);
    vm.resume_generator(id, mode, value)
}

pub(super) fn next(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    resume(vm, this, args, "next", ResumeMode::Next)
}

pub(super) fn return_(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    resume(vm, this, args, "return", ResumeMode::Return)
}

pub(super) fn throw(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    resume(vm, this, args, "throw", ResumeMode::Throw)
}
