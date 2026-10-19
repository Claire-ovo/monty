//! Global functions and values: `parseInt`, `parseFloat`, `isNaN`,
//! `isFinite`, `globalThis`, `undefined`, `NaN`, `Infinity`.

use super::{NativeFn, arg, install_function, install_value};
use crate::{
    error::RunResult,
    heap::{HeapId, PropFlags},
    value::JsValue,
    vm::{Vm, number::is_js_whitespace},
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    for native in [
        NativeFn::ParseInt,
        NativeFn::ParseFloat,
        NativeFn::IsNaN,
        NativeFn::IsFinite,
    ] {
        install_function(vm, global, native);
    }
    // `Number.parseFloat` and `Number.parseInt` are the same functions as the globals
    let number_key = super::key(vm, "Number");
    if let Ok(JsValue::Ref(number)) = vm.get(&JsValue::Ref(global), number_key) {
        for name in ["parseFloat", "parseInt"] {
            let key = super::key(vm, name);
            if let Ok(function) = vm.get(&JsValue::Ref(global), key) {
                vm.define_with_flags(number, key, function, PropFlags::HIDDEN);
            }
        }
        vm.session.heap.dec_ref(number);
    }
    install_function(vm, global, NativeFn::BigInt);
    vm.session.heap.inc_ref(global);
    install_value(vm, global, "globalThis", JsValue::Ref(global), PropFlags::HIDDEN);
    install_value(vm, global, "undefined", JsValue::Undefined, PropFlags::NONE);
    install_value(vm, global, "NaN", JsValue::Number(f64::NAN), PropFlags::NONE);
    install_value(vm, global, "Infinity", JsValue::Number(f64::INFINITY), PropFlags::NONE);
}

pub(super) fn parse_int(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let text = vm.coerce_string(arg(args, 0))?.to_string_lossy();
    let radix = vm.coerce_integer(arg(args, 1))?;
    Ok(JsValue::Number(parse_int_text(&text, radix)))
}

/// `parseInt`'s algorithm: optional sign, a radix prefix, then as many
/// digits as parse.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "radix is range-checked"
)]
pub(crate) fn parse_int_text(text: &str, radix: f64) -> f64 {
    let trimmed = text.trim_start_matches(is_js_whitespace);
    let (sign, rest) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1.0, &trimmed[1..]),
        Some(b'+') => (1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    #[expect(clippy::cast_possible_truncation, reason = "out-of-range radixes are rejected below")]
    let mut radix = if radix == 0.0 { 10 } else { radix as i64 };
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let mut rest = rest;
    if (radix == 16
        || text
            .trim_start_matches(is_js_whitespace)
            .trim_start_matches(['-', '+'])
            .starts_with("0x"))
        && (rest.starts_with("0x") || rest.starts_with("0X"))
        && (radix == 16 || radix == 10 && !is_explicit_radix(text, radix))
    {
        rest = &rest[2..];
        radix = 16;
    }
    let radix = radix as u32;
    let mut value = 0.0f64;
    let mut any = false;
    for c in rest.chars() {
        match c.to_digit(radix) {
            Some(d) => {
                value = value * f64::from(radix) + f64::from(d);
                any = true;
            }
            None => break,
        }
    }
    if any { sign * value } else { f64::NAN }
}

/// Whether the caller passed a radix (so `0x` is only honoured for 16).
fn is_explicit_radix(_: &str, radix: i64) -> bool {
    radix != 10
}

pub(super) fn parse_float(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let text = vm.coerce_string(arg(args, 0))?.to_string_lossy();
    Ok(JsValue::Number(parse_float_text(&text)))
}

/// `parseFloat`: the longest `StrDecimalLiteral` prefix.
pub(crate) fn parse_float_text(text: &str) -> f64 {
    let trimmed = text.trim_start_matches(is_js_whitespace);
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    if trimmed[i..].starts_with("Infinity") {
        return if bytes.first() == Some(&b'-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits_start || (i == digits_start + 1 && bytes[digits_start] == b'.') {
        return f64::NAN;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    trimmed[..i].parse::<f64>().unwrap_or(f64::NAN)
}

/// `BigInt` exists so `typeof BigInt` and feature checks behave, but every
/// call is refused: the engine has no arbitrary-precision integers.
pub(super) fn big_int(vm: &mut Vm<'_>, _: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    Err(vm.type_error("BigInt is not supported"))
}

pub(super) fn is_nan(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let number = vm.coerce_number(arg(args, 0))?;
    Ok(JsValue::Bool(number.is_nan()))
}

pub(super) fn is_finite(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let number = vm.coerce_number(arg(args, 0))?;
    Ok(JsValue::Bool(number.is_finite()))
}
