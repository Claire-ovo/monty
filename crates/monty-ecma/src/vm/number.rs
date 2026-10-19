//! Number ↔ string conversions with ECMAScript's exact formatting rules.

use std::{fmt::Write, str};

/// `Number::toString()` (ECMA-262 `Number::toString(x, 10)`): the shortest
/// digit string that round-trips, in positional notation for exponents in
/// `-7..21` and exponential notation outside.
pub(crate) fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let negative = value < 0.0;
    let (digits, exponent) = shortest_digits(value.abs());
    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }
    // `n` is the position of the decimal point relative to the digit string:
    // the value is 0.d₁d₂… × 10ⁿ, with `k` significant digits
    let k = i32::try_from(digits.len()).unwrap_or(i32::MAX);
    let n = exponent + 1;
    if (k..=21).contains(&n) {
        out.push_str(&digits);
        for _ in 0..n - k {
            out.push('0');
        }
    } else if (1..=21).contains(&n) {
        let split = usize::try_from(n).unwrap_or(0);
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    } else if (-5..=0).contains(&n) {
        out.push_str("0.");
        for _ in 0..-n {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e >= 0 { '+' } else { '-' });
        write!(out, "{}", e.abs()).expect("writing to a string");
    }
    out
}

/// The shortest round-tripping decimal digits of a positive finite number
/// and the decimal exponent of its first digit (`d.ddd × 10^exp`).
fn shortest_digits(value: f64) -> (String, i32) {
    // Rust's `{:e}` is the shortest representation that round-trips
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted.split_once('e').expect("exponential format has an exponent");
    let exponent: i32 = exponent.parse().expect("exponent is an integer");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    (digits.to_owned(), exponent)
}

/// `Number::toString(x, radix)` for radix 2..=36 (an integer or fraction in that base).
pub(crate) fn number_to_string_radix(value: f64, radix: u32) -> String {
    if radix == 10 || !value.is_finite() {
        return number_to_string(value);
    }
    let negative = value < 0.0;
    let value = value.abs();
    let mut integer = value.trunc();
    let mut fraction = value - integer;
    let mut int_digits = Vec::new();
    if integer == 0.0 {
        int_digits.push(b'0');
    }
    let radix_f = f64::from(radix);
    while integer >= 1.0 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a remainder below the radix"
        )]
        let digit = (integer % radix_f) as u32;
        int_digits.push(digit_char(digit));
        integer = (integer / radix_f).trunc();
    }
    int_digits.reverse();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(str::from_utf8(&int_digits).expect("ascii digits"));
    if fraction > 0.0 {
        out.push('.');
        // enough digits to distinguish neighbouring doubles, as engines do
        let mut remaining = 52;
        while fraction > 0.0 && remaining > 0 {
            fraction *= radix_f;
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a digit below the radix"
            )]
            let digit = fraction.trunc() as u32;
            out.push(char::from(digit_char(digit)));
            fraction -= fraction.trunc();
            remaining -= 1;
        }
    }
    out
}

fn digit_char(digit: u32) -> u8 {
    b"0123456789abcdefghijklmnopqrstuvwxyz"[digit as usize]
}

/// `StringToNumber`: the numeric value of a string, `NaN` when it is not a
/// `StrDecimalLiteral`, a `0x`/`0o`/`0b` literal, or whitespace (which is `0`).
pub(crate) fn string_to_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(is_js_whitespace);
    if trimmed.is_empty() {
        return 0.0;
    }
    let (sign, body) = match trimmed.as_bytes()[0] {
        b'-' => (-1.0, &trimmed[1..]),
        b'+' => (1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return if sign < 0.0 { f64::NAN } else { parse_radix(rest, 16) };
    }
    if let Some(rest) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        return if sign < 0.0 { f64::NAN } else { parse_radix(rest, 8) };
    }
    if let Some(rest) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        return if sign < 0.0 { f64::NAN } else { parse_radix(rest, 2) };
    }
    if !is_decimal_literal(body) {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |v| sign * v)
}

/// Whether `body` is a `StrUnsignedDecimalLiteral`: digits with an optional
/// fraction and exponent, no leading sign, not Rust's `inf`/`nan` spellings.
fn is_decimal_literal(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == bytes.len()
}

/// The value of non-empty digits in `radix`; `NaN` on any other character.
fn parse_radix(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    let mut value = 0.0f64;
    for c in digits.chars() {
        match c.to_digit(radix) {
            Some(d) => value = value * f64::from(radix) + f64::from(d),
            None => return f64::NAN,
        }
    }
    value
}

/// ECMAScript `WhiteSpace` and `LineTerminator` code points.
pub(crate) fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// A non-negative integral `f64` (a clamped `ToIntegerOrInfinity` result) as
/// an index; negative or NaN values become 0 and huge ones saturate.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to the usize range first"
)]
pub(crate) fn to_usize(value: f64) -> usize {
    if value.is_nan() || value <= 0.0 {
        0
    } else if value >= usize::MAX as f64 {
        usize::MAX
    } else {
        value as usize
    }
}

/// A `usize` index as a property-key index; indices beyond `u32` saturate
/// (no array can hold that many elements).
pub(crate) fn index_u32(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

/// `ToInt32`: modulo 2³² into the signed range.
#[expect(
    clippy::cast_possible_wrap,
    reason = "the two's complement reinterpretation is the spec"
)]
pub(crate) fn to_int32(value: f64) -> i32 {
    to_uint32(value) as i32
}

/// `ToUint32`: modulo 2³².
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the modulo keeps the value in range"
)]
pub(crate) fn to_uint32(value: f64) -> u32 {
    if !value.is_finite() {
        return 0;
    }
    let truncated = value.trunc();
    let modulo = truncated.rem_euclid(4_294_967_296.0);
    modulo as u32
}

/// `Number.prototype.toFixed(digits)`.
pub(crate) fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        number_to_string(value)
    } else {
        let text = format!("{value:.digits$}");
        // Rust prints `-0.00` for a negative value rounding to zero; the spec keeps the sign only for x < 0
        if value == 0.0 && text.starts_with('-') {
            text[1..].to_owned()
        } else {
            text
        }
    }
}

/// `Number.prototype.toPrecision(precision)`.
pub(crate) fn to_precision(value: f64, precision: usize) -> String {
    if !value.is_finite() || value == 0.0 {
        return if value == 0.0 && precision > 1 {
            format!("0.{}", "0".repeat(precision - 1))
        } else {
            number_to_string(value)
        };
    }
    let exponent = value.abs().log10().floor();
    let formatted = format!("{:.*e}", precision - 1, value);
    let (mantissa, exp) = formatted.split_once('e').expect("exponential format");
    let exp: i32 = exp.parse().expect("integer exponent");
    let _ = exponent;
    if exp < -6 || exp >= i32::try_from(precision).unwrap_or(i32::MAX) {
        let sign = if exp >= 0 { "+" } else { "-" };
        format!("{mantissa}e{sign}{}", exp.abs())
    } else {
        format!(
            "{value:.*}",
            usize::try_from(i32::try_from(precision).unwrap_or(0) - 1 - exp).unwrap_or(0)
        )
    }
}
