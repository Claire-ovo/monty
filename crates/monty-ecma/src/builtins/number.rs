//! `Number`, `Boolean` and `Symbol` with their prototypes.

use super::{NativeFn, arg, install_constructor, install_function, install_getter, install_value, raw_object};
use crate::{
    error::RunResult,
    heap::{HeapId, ObjectClass, PropFlags, PropertyKey},
    intern::{SymbolId, WellKnownSymbol},
    string::JsString,
    value::JsValue,
    vm::{
        Vm, number,
        number::{index_u32, to_usize},
    },
};

/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    // Number
    let number_prototype = raw_object(vm, Some(object_prototype));
    vm.object_mut(number_prototype).expect("object").class = ObjectClass::BoxedNumber(0.0);
    vm.session.realm.number_prototype = number_prototype;
    for native in [
        NativeFn::NumberProtoToString,
        NativeFn::NumberProtoToFixed,
        NativeFn::NumberProtoToPrecision,
        NativeFn::NumberProtoValueOf,
    ] {
        install_function(vm, number_prototype, native);
    }
    let number = install_constructor(vm, global, NativeFn::Number, number_prototype);
    for native in [
        NativeFn::NumberIsInteger,
        NativeFn::NumberIsFinite,
        NativeFn::NumberIsNaN,
        NativeFn::NumberIsSafeInteger,
    ] {
        install_function(vm, number, native);
    }
    for (name, value) in [
        ("MAX_SAFE_INTEGER", MAX_SAFE_INTEGER),
        ("MIN_SAFE_INTEGER", -MAX_SAFE_INTEGER),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("EPSILON", f64::EPSILON),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ] {
        install_value(vm, number, name, JsValue::Number(value), PropFlags::NONE);
    }
    // `Number.parseFloat` / `parseInt` are the globals
    let global_value = JsValue::Ref(global);
    for name in ["parseFloat", "parseInt"] {
        let key = super::key(vm, name);
        if let Ok(function) = vm.get(&global_value, key) {
            vm.define_with_flags(number, key, function, PropFlags::HIDDEN);
        }
    }
    // Boolean
    let boolean_prototype = raw_object(vm, Some(object_prototype));
    vm.object_mut(boolean_prototype).expect("object").class = ObjectClass::BoxedBool(false);
    vm.session.realm.boolean_prototype = boolean_prototype;
    install_function(vm, boolean_prototype, NativeFn::BooleanProtoToString);
    install_function(vm, boolean_prototype, NativeFn::BooleanProtoValueOf);
    install_constructor(vm, global, NativeFn::Boolean, boolean_prototype);
    // Symbol
    let symbol_prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.symbol_prototype = symbol_prototype;
    install_function(vm, symbol_prototype, NativeFn::SymbolProtoToString);
    install_getter(vm, symbol_prototype, "description", NativeFn::SymbolProtoDescription);
    let symbol = install_constructor(vm, global, NativeFn::Symbol, symbol_prototype);
    for well_known in <WellKnownSymbol as strum::IntoEnumIterator>::iter() {
        let name: &'static str = well_known.into();
        install_value(vm, symbol, name, JsValue::Symbol(well_known.id()), PropFlags::NONE);
    }
}

pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    let value = match args.first() {
        None => 0.0,
        Some(value) => vm.coerce_number(value)?,
    };
    if construct {
        let proto = vm.session.realm.number_prototype;
        Ok(JsValue::Ref(
            vm.new_class_object(proto, ObjectClass::BoxedNumber(value)),
        ))
    } else {
        Ok(JsValue::Number(value))
    }
}

pub(super) fn is_integer(_: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Bool(
        matches!(arg(args, 0), JsValue::Number(n) if n.is_finite() && n.fract() == 0.0),
    ))
}

pub(super) fn is_finite(_: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Bool(
        matches!(arg(args, 0), JsValue::Number(n) if n.is_finite()),
    ))
}

pub(super) fn is_nan(_: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Bool(matches!(arg(args, 0), JsValue::Number(n) if n.is_nan())))
}

pub(super) fn is_safe_integer(_: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Bool(
        matches!(arg(args, 0), JsValue::Number(n) if n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER),
    ))
}

/// The number a `Number.prototype` method was called on.
fn this_number(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<f64> {
    match this {
        JsValue::Number(n) => Ok(*n),
        JsValue::Ref(id) => match vm.object(*id).map(|o| &o.class) {
            Some(ObjectClass::BoxedNumber(n)) => Ok(*n),
            _ => Err(vm.type_error(&format!("Number.prototype.{method} requires that 'this' be a Number"))),
        },
        _ => Err(vm.type_error(&format!("Number.prototype.{method} requires that 'this' be a Number"))),
    }
}

pub(super) fn to_string(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = this_number(vm, this, "toString")?;
    let radix = match arg(args, 0) {
        JsValue::Undefined => 10.0,
        other => vm.coerce_integer(other)?,
    };
    if !(2.0..=36.0).contains(&radix) {
        return Err(vm.range_error("toString() radix must be between 2 and 36"));
    }
    let text = number::number_to_string_radix(value, index_u32(to_usize(radix)));
    vm.alloc_string(JsString::from(text.as_str()))
}

pub(super) fn to_fixed(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = this_number(vm, this, "toFixed")?;
    let digits = vm.coerce_integer(arg(args, 0))?;
    if !(0.0..=100.0).contains(&digits) {
        return Err(vm.range_error("toFixed() digits argument must be between 0 and 100"));
    }
    let text = number::to_fixed(value, to_usize(digits));
    vm.alloc_string(JsString::from(text.as_str()))
}

pub(super) fn to_precision(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = this_number(vm, this, "toPrecision")?;
    if *arg(args, 0) == JsValue::Undefined {
        let text = number::number_to_string(value);
        return vm.alloc_string(JsString::from(text.as_str()));
    }
    let precision = vm.coerce_integer(arg(args, 0))?;
    if !(1.0..=100.0).contains(&precision) {
        return Err(vm.range_error("toPrecision() argument must be between 1 and 100"));
    }
    let text = number::to_precision(value, to_usize(precision));
    vm.alloc_string(JsString::from(text.as_str()))
}

pub(super) fn value_of(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    this_number(vm, this, "valueOf").map(JsValue::Number)
}

pub(super) fn boolean_constructor(
    vm: &mut Vm<'_>,
    _: &JsValue,
    args: &[JsValue],
    construct: bool,
) -> RunResult<JsValue> {
    let value = vm.to_boolean(arg(args, 0));
    if construct {
        let proto = vm.session.realm.boolean_prototype;
        Ok(JsValue::Ref(vm.new_class_object(proto, ObjectClass::BoxedBool(value))))
    } else {
        Ok(JsValue::Bool(value))
    }
}

fn this_boolean(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<bool> {
    match this {
        JsValue::Bool(b) => Ok(*b),
        JsValue::Ref(id) => match vm.object(*id).map(|o| &o.class) {
            Some(ObjectClass::BoxedBool(b)) => Ok(*b),
            _ => Err(vm.type_error(&format!("Boolean.prototype.{method} requires that 'this' be a Boolean"))),
        },
        _ => Err(vm.type_error(&format!("Boolean.prototype.{method} requires that 'this' be a Boolean"))),
    }
}

pub(super) fn boolean_to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = this_boolean(vm, this, "toString")?;
    Ok(vm.atom_value(if value { "true" } else { "false" }))
}

pub(super) fn boolean_value_of(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    this_boolean(vm, this, "valueOf").map(JsValue::Bool)
}

pub(super) fn symbol_constructor(
    vm: &mut Vm<'_>,
    _: &JsValue,
    args: &[JsValue],
    construct: bool,
) -> RunResult<JsValue> {
    if construct {
        return Err(vm.type_error("Symbol is not a constructor"));
    }
    let description = match arg(args, 0) {
        JsValue::Undefined => None,
        other => Some(vm.coerce_string(other)?),
    };
    Ok(JsValue::Symbol(vm.session.interns.new_symbol(description)))
}

fn this_symbol(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<SymbolId> {
    match this {
        JsValue::Symbol(s) => Ok(*s),
        JsValue::Ref(id) => match vm.object(*id).map(|o| &o.class) {
            Some(ObjectClass::BoxedSymbol(s)) => Ok(*s),
            _ => Err(vm.type_error(&format!("Symbol.prototype.{method} requires that 'this' be a Symbol"))),
        },
        _ => Err(vm.type_error(&format!("Symbol.prototype.{method} requires that 'this' be a Symbol"))),
    }
}

pub(super) fn symbol_to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let symbol = this_symbol(vm, this, "toString")?;
    let text = vm.key_display(PropertyKey::Symbol(symbol));
    vm.alloc_string(JsString::from(text.as_str()))
}

pub(super) fn symbol_description(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let symbol = this_symbol(vm, this, "description")?;
    match vm.session.interns.symbol(symbol).description.clone() {
        Some(description) => vm.alloc_string(description),
        None => Ok(JsValue::Undefined),
    }
}
