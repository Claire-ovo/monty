//! `Function.prototype`: `call`, `apply`, `bind`, `toString`.

use super::{NativeFn, arg, install_constructor, install_function, require_callable};
use crate::{
    error::RunResult,
    heap::{FunctionObject, HeapId, ObjectClass, PropFlags, PropertyKey},
    intern::StaticAtom,
    realm::ErrorKind,
    string::JsString,
    value::JsValue,
    vm::{
        Vm,
        number::{index_u32, to_usize},
    },
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId, prototype: HeapId) {
    for native in [
        NativeFn::FunctionProtoCall,
        NativeFn::FunctionProtoApply,
        NativeFn::FunctionProtoBind,
    ] {
        install_function(vm, prototype, native);
    }
    install_function(vm, prototype, NativeFn::FunctionProtoToString);
    super::install_value(
        vm,
        prototype,
        "name",
        JsValue::Atom(StaticAtom::Empty.id()),
        PropFlags::CONFIGURABLE,
    );
    super::install_value(vm, prototype, "length", JsValue::Number(0.0), PropFlags::CONFIGURABLE);
    install_constructor(vm, global, NativeFn::Function, prototype);
}

/// `new Function(...)` compiles source at runtime, which the sandbox does not offer.
pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    Err(vm.throw_error(ErrorKind::EvalError, "Code generation from strings is not supported"))
}

pub(super) fn proto_call(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    require_callable(vm, this)?;
    let receiver = arg(args, 0).clone_with_heap(&vm.session.heap);
    let rest: Vec<JsValue> = args
        .iter()
        .skip(1)
        .map(|a| a.clone_with_heap(&vm.session.heap))
        .collect();
    vm.call(this.clone_with_heap(&vm.session.heap), receiver, rest)
}

pub(super) fn proto_apply(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    require_callable(vm, this)?;
    let receiver = arg(args, 0).clone_with_heap(&vm.session.heap);
    let list = match arg(args, 1) {
        JsValue::Undefined | JsValue::Null => Vec::new(),
        array @ JsValue::Ref(_) => {
            let array = array.clone_with_heap(&vm.session.heap);
            array_like_to_vec(vm, array)?
        }
        _ => {
            receiver.drop_with(&mut vm.session.heap);
            return Err(vm.type_error("CreateListFromArrayLike called on non-object"));
        }
    };
    vm.call(this.clone_with_heap(&vm.session.heap), receiver, list)
}

/// The elements of an array or array-like (`length` + indices), consuming it.
pub(crate) fn array_like_to_vec(vm: &mut Vm<'_>, value: JsValue) -> RunResult<Vec<JsValue>> {
    if vm.is_array(&value) {
        return vm.array_to_vec(value);
    }
    let length = vm.get_atom(&value, StaticAtom::Length.id())?;
    let length = to_usize(vm.coerce_integer(&length)?);
    let mut items = Vec::with_capacity(length.min(1024));
    for index in 0..length {
        vm.tracker().check_growth(items.len(), items.capacity(), 16)?;
        items.push(vm.get(&value, PropertyKey::Index(index_u32(index)))?);
    }
    value.drop_with(&mut vm.session.heap);
    Ok(items)
}

pub(super) fn proto_bind(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    require_callable(vm, this)?;
    let JsValue::Ref(target) = this else { unreachable!() };
    let bound_this = arg(args, 0).clone_with_heap(&vm.session.heap);
    let bound_args: Vec<JsValue> = args
        .iter()
        .skip(1)
        .map(|a| a.clone_with_heap(&vm.session.heap))
        .collect();
    let target_name = vm.get_atom(this, StaticAtom::Name.id())?;
    let name_text = vm
        .string_of(&target_name)
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    target_name.drop_with(&mut vm.session.heap);
    let target_length = vm.get_atom(this, StaticAtom::Length.id())?;
    let length = match target_length {
        JsValue::Number(n) => u16::try_from(to_usize(n - bound_args.len() as f64)).unwrap_or(u16::MAX),
        _ => 0,
    };
    vm.session.heap.inc_ref(*target);
    let callable = FunctionObject::Bound {
        target: *target,
        this: bound_this,
        args: bound_args.into_boxed_slice(),
    };
    let name = vm.session.interns.intern_str(&format!("bound {name_text}"));
    let function = vm.new_function_object(callable, name, length);
    Ok(JsValue::Ref(function))
}

/// `Function.prototype.toString`: natives print as `function name() { [native code] }`,
/// compiled functions as their source text.
pub(super) fn proto_to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let JsValue::Ref(id) = this else {
        return Err(vm.type_error("Function.prototype.toString requires that 'this' be a Function"));
    };
    let text = match vm.object(*id).map(|o| &o.class) {
        Some(ObjectClass::Function(FunctionObject::Closure { func, .. })) => {
            let function = vm.session.interns.function(*func);
            let span = function.span;
            vm.session.interns.source(span.feed as usize).map_or_else(
                || "function () { [native code] }".to_owned(),
                |source| source[span.start as usize..span.end as usize].to_owned(),
            )
        }
        Some(ObjectClass::Function(FunctionObject::Native(native))) => {
            format!("function {}() {{ [native code] }}", native.name())
        }
        Some(ObjectClass::Function(FunctionObject::Bound { .. })) => "function () { [native code] }".to_owned(),
        _ => return Err(vm.type_error("Function.prototype.toString requires that 'this' be a Function")),
    };
    vm.alloc_string(JsString::from(text.as_str()))
}
