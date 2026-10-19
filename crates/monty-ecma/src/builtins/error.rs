//! The `Error` constructors and `Error.prototype`.

use super::{NativeFn, arg, install_constructor, install_function, install_value, raw_object};
use crate::{
    error::RunResult,
    heap::{HeapId, PropFlags, PropertyKey},
    intern::StaticAtom,
    realm::ErrorKind,
    string::JsString,
    value::JsValue,
    vm::Vm,
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    let error_prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.error_prototype = error_prototype;
    install_function(vm, error_prototype, NativeFn::ErrorProtoToString);
    install_value(
        vm,
        error_prototype,
        "name",
        JsValue::Atom(StaticAtom::Error.id()),
        PropFlags::HIDDEN,
    );
    install_value(
        vm,
        error_prototype,
        "message",
        JsValue::Atom(StaticAtom::Empty.id()),
        PropFlags::HIDDEN,
    );
    let error = install_constructor(vm, global, NativeFn::Error, error_prototype);
    for (kind, native) in [
        (ErrorKind::TypeError, NativeFn::TypeError),
        (ErrorKind::RangeError, NativeFn::RangeError),
        (ErrorKind::ReferenceError, NativeFn::ReferenceError),
        (ErrorKind::SyntaxError, NativeFn::SyntaxError),
        (ErrorKind::EvalError, NativeFn::EvalError),
        (ErrorKind::UriError, NativeFn::UriError),
        (ErrorKind::AggregateError, NativeFn::AggregateError),
    ] {
        let prototype = raw_object(vm, Some(error_prototype));
        let name = vm.atom_value(kind.name());
        install_value(vm, prototype, "name", name, PropFlags::HIDDEN);
        install_value(
            vm,
            prototype,
            "message",
            JsValue::Atom(StaticAtom::Empty.id()),
            PropFlags::HIDDEN,
        );
        let constructor = install_constructor(vm, global, native, prototype);
        // subclass constructors inherit from `Error` itself
        vm.set_prototype(constructor, Some(error));
        match kind {
            ErrorKind::TypeError => vm.session.realm.type_error_prototype = prototype,
            ErrorKind::RangeError => vm.session.realm.range_error_prototype = prototype,
            ErrorKind::ReferenceError => vm.session.realm.reference_error_prototype = prototype,
            ErrorKind::SyntaxError => vm.session.realm.syntax_error_prototype = prototype,
            ErrorKind::EvalError => vm.session.realm.eval_error_prototype = prototype,
            ErrorKind::UriError => vm.session.realm.uri_error_prototype = prototype,
            ErrorKind::AggregateError => vm.session.realm.aggregate_error_prototype = prototype,
            ErrorKind::Error => {}
        }
    }
}

pub(super) fn error(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    construct_error(vm, ErrorKind::Error, this, args, construct)
}

pub(super) fn type_error(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    construct_error(vm, ErrorKind::TypeError, this, args, construct)
}

pub(super) fn range_error(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    construct_error(vm, ErrorKind::RangeError, this, args, construct)
}

pub(super) fn reference_error(
    vm: &mut Vm<'_>,
    this: &JsValue,
    args: &[JsValue],
    construct: bool,
) -> RunResult<JsValue> {
    construct_error(vm, ErrorKind::ReferenceError, this, args, construct)
}

pub(super) fn syntax_error(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    construct_error(vm, ErrorKind::SyntaxError, this, args, construct)
}

pub(super) fn eval_error(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    construct_error(vm, ErrorKind::EvalError, this, args, construct)
}

pub(super) fn uri_error(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    construct_error(vm, ErrorKind::UriError, this, args, construct)
}

pub(super) fn aggregate_error(
    vm: &mut Vm<'_>,
    this: &JsValue,
    args: &[JsValue],
    construct: bool,
) -> RunResult<JsValue> {
    // AggregateError(errors, message, options)
    let shifted: Vec<JsValue> = args
        .iter()
        .skip(1)
        .map(|a| a.clone_with_heap(&vm.session.heap))
        .collect();
    let error = construct_error(vm, ErrorKind::AggregateError, this, &shifted, construct);
    for value in shifted {
        value.drop_with(&mut vm.session.heap);
    }
    let error = error?;
    if let JsValue::Ref(id) = &error {
        let errors = arg(args, 0).clone_with_heap(&vm.session.heap);
        let list = vm.iterate_to_vec(errors)?;
        let array = vm.new_array(list)?;
        vm.define_with_flags(
            *id,
            PropertyKey::Atom(StaticAtom::Errors.id()),
            array,
            PropFlags::HIDDEN,
        );
    }
    Ok(error)
}

/// `new Error(message, { cause })`, also callable without `new`.
fn construct_error(
    vm: &mut Vm<'_>,
    kind: ErrorKind,
    _this: &JsValue,
    args: &[JsValue],
    _construct: bool,
) -> RunResult<JsValue> {
    let message = match arg(args, 0) {
        JsValue::Undefined => String::new(),
        value => vm.coerce_string(value)?.to_string_lossy(),
    };
    let error = vm.new_error(kind, &message);
    if let JsValue::Ref(id) = &error {
        if arg(args, 0) == &JsValue::Undefined {
            // no own `message` when none was given
            let key = PropertyKey::Atom(StaticAtom::Message.id());
            let _ = vm.delete(&JsValue::Ref(*id), key);
        }
        if let options @ JsValue::Ref(_) = arg(args, 1) {
            let cause_key = PropertyKey::Atom(StaticAtom::Cause.id());
            if let JsValue::Ref(options_id) = options
                && vm.has_property(*options_id, cause_key)
            {
                let cause = vm.get(options, cause_key)?;
                vm.define_with_flags(*id, cause_key, cause, PropFlags::HIDDEN);
            }
        }
    }
    Ok(error)
}

/// `Error.prototype.toString`: `name: message`.
pub(super) fn proto_to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    if !this.is_object() {
        return Err(vm.type_error("Error.prototype.toString requires that 'this' be an Object"));
    }
    let name = vm.get_atom(this, StaticAtom::Name.id())?;
    let name_text = if name == JsValue::Undefined {
        "Error".to_owned()
    } else {
        vm.coerce_string(&name)?.to_string_lossy()
    };
    name.drop_with(&mut vm.session.heap);
    let message = vm.get_atom(this, StaticAtom::Message.id())?;
    let message_text = if message == JsValue::Undefined {
        String::new()
    } else {
        vm.coerce_string(&message)?.to_string_lossy()
    };
    message.drop_with(&mut vm.session.heap);
    let text = if name_text.is_empty() {
        message_text
    } else if message_text.is_empty() {
        name_text
    } else {
        format!("{name_text}: {message_text}")
    };
    vm.alloc_string(JsString::from(text.as_str()))
}
