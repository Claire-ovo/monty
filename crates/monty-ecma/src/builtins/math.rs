//! `Math`.

use std::f64::consts;

use super::{NativeFn, arg, install_function, install_value, plain_object};
use crate::{
    error::RunResult,
    heap::{HeapId, PropFlags},
    value::JsValue,
    vm::{Vm, ops::js_pow},
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let math = plain_object(vm);
    for native in [
        NativeFn::MathAbs,
        NativeFn::MathFloor,
        NativeFn::MathCeil,
        NativeFn::MathRound,
        NativeFn::MathTrunc,
        NativeFn::MathSign,
        NativeFn::MathSqrt,
        NativeFn::MathCbrt,
        NativeFn::MathPow,
        NativeFn::MathMin,
        NativeFn::MathMax,
        NativeFn::MathRandom,
        NativeFn::MathLog,
        NativeFn::MathLog2,
        NativeFn::MathLog10,
        NativeFn::MathExp,
        NativeFn::MathSin,
        NativeFn::MathCos,
        NativeFn::MathTan,
        NativeFn::MathAsin,
        NativeFn::MathAcos,
        NativeFn::MathAtan,
        NativeFn::MathAtan2,
        NativeFn::MathHypot,
    ] {
        install_function(vm, math, native);
    }
    for (name, value) in [
        ("PI", consts::PI),
        ("E", consts::E),
        ("LN2", consts::LN_2),
        ("LN10", consts::LN_10),
        ("LOG2E", consts::LOG2_E),
        ("LOG10E", consts::LOG10_E),
        ("SQRT2", consts::SQRT_2),
        ("SQRT1_2", consts::FRAC_1_SQRT_2),
    ] {
        install_value(vm, math, name, JsValue::Number(value), PropFlags::NONE);
    }
    install_value(vm, global, "Math", JsValue::Ref(math), PropFlags::HIDDEN);
}

/// Declares one-argument `Math` functions.
macro_rules! unary {
    ($($name:ident => $op:expr),* $(,)?) => {
        $(
            pub(super) fn $name(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
                let x = vm.coerce_number(arg(args, 0))?;
                let f: fn(f64) -> f64 = $op;
                Ok(JsValue::Number(f(x)))
            }
        )*
    };
}

unary! {
    abs => f64::abs,
    floor => f64::floor,
    ceil => f64::ceil,
    round => js_round,
    trunc => f64::trunc,
    sign => |x| if x.is_nan() || x == 0.0 { x } else { x.signum() },
    sqrt => f64::sqrt,
    cbrt => f64::cbrt,
    log => f64::ln,
    log2 => f64::log2,
    log10 => f64::log10,
    exp => f64::exp,
    sin => f64::sin,
    cos => f64::cos,
    tan => f64::tan,
    asin => f64::asin,
    acos => f64::acos,
    atan => f64::atan,
}

/// `Math.round`: halves round toward +∞, and `-0.4` rounds to `-0`.
fn js_round(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() || x.fract() == 0.0 {
        x
    } else if (-0.5..0.0).contains(&x) {
        -0.0
    } else {
        (x + 0.5).floor()
    }
}

pub(super) fn pow(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let x = vm.coerce_number(arg(args, 0))?;
    let y = vm.coerce_number(arg(args, 1))?;
    Ok(JsValue::Number(js_pow(x, y)))
}

pub(super) fn atan2(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let y = vm.coerce_number(arg(args, 0))?;
    let x = vm.coerce_number(arg(args, 1))?;
    Ok(JsValue::Number(y.atan2(x)))
}

pub(super) fn hypot(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let mut sum = 0.0f64;
    for value in args {
        let n = vm.coerce_number(value)?;
        sum += n * n;
    }
    Ok(JsValue::Number(sum.sqrt()))
}

pub(super) fn min(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let mut result = f64::INFINITY;
    for value in args {
        let n = vm.coerce_number(value)?;
        if n.is_nan() {
            return Ok(JsValue::Number(f64::NAN));
        }
        if n < result || (n == 0.0 && result == 0.0 && n.is_sign_negative()) {
            result = n;
        }
    }
    Ok(JsValue::Number(result))
}

pub(super) fn max(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let mut result = f64::NEG_INFINITY;
    for value in args {
        let n = vm.coerce_number(value)?;
        if n.is_nan() {
            return Ok(JsValue::Number(f64::NAN));
        }
        if n > result || (n == 0.0 && result == 0.0 && result.is_sign_negative()) {
            result = n;
        }
    }
    Ok(JsValue::Number(result))
}

/// `Math.random`: a session-local xorshift generator.
// TODO(os-policy): seed from `OsPolicy::random_start` as Python's `random` does.
pub(super) fn random(vm: &mut Vm<'_>, _: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let state = &mut vm.session.random_state;
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    #[expect(clippy::cast_precision_loss, reason = "53 random bits become the mantissa")]
    let value = (x >> 11) as f64 / (1u64 << 53) as f64;
    Ok(JsValue::Number(value))
}
