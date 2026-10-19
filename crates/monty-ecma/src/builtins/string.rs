//! `String` and `String.prototype`, over UTF-16 code units.

use std::{cmp::Ordering, slice};

use monty_regexp::unicode::{self, normalize::NormalizationForm};

use super::{
    NativeFn, NativeImpl, arg, install_constructor, install_function, install_symbol_function, integer_arg, key,
    raw_object, regexp,
};
use crate::{
    error::RunResult,
    heap::{HeapId, ObjectClass, PropFlags, PropertyKey},
    intern::WellKnownSymbol,
    string::JsString,
    value::JsValue,
    vm::{
        Vm,
        number::{is_js_whitespace, number_to_string, to_uint32, to_usize},
    },
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    let prototype = raw_object(vm, Some(object_prototype));
    vm.object_mut(prototype).expect("object").class = ObjectClass::BoxedString(JsString::empty());
    vm.session.realm.string_prototype = prototype;
    for native in [
        NativeFn::StringProtoCharAt,
        NativeFn::StringProtoCharCodeAt,
        NativeFn::StringProtoCodePointAt,
        NativeFn::StringProtoIndexOf,
        NativeFn::StringProtoLastIndexOf,
        NativeFn::StringProtoIncludes,
        NativeFn::StringProtoStartsWith,
        NativeFn::StringProtoEndsWith,
        NativeFn::StringProtoSlice,
        NativeFn::StringProtoSubstring,
        NativeFn::StringProtoToUpperCase,
        NativeFn::StringProtoToLowerCase,
        NativeFn::StringProtoTrim,
        NativeFn::StringProtoTrimStart,
        NativeFn::StringProtoTrimEnd,
        NativeFn::StringProtoSplit,
        NativeFn::StringProtoRepeat,
        NativeFn::StringProtoPadStart,
        NativeFn::StringProtoPadEnd,
        NativeFn::StringProtoReplace,
        NativeFn::StringProtoReplaceAll,
        NativeFn::StringProtoMatch,
        NativeFn::StringProtoMatchAll,
        NativeFn::StringProtoSearch,
        NativeFn::StringProtoNormalize,
        NativeFn::StringProtoAt,
        NativeFn::StringProtoConcat,
        NativeFn::StringProtoToString,
        NativeFn::StringProtoLocaleCompare,
    ] {
        install_function(vm, prototype, native);
    }
    // `valueOf` is the same function as `toString`
    let value_of = vm.session.interns.intern_str("valueOf");
    let to_string = vm.session.interns.intern_str("toString");
    if let Ok(function) = vm.get(&JsValue::Ref(prototype), PropertyKey::Atom(to_string)) {
        vm.define_with_flags(prototype, PropertyKey::Atom(value_of), function, PropFlags::HIDDEN);
    }
    install_symbol_function(vm, prototype, WellKnownSymbol::Iterator, NativeFn::StringProtoIterator);
    let constructor = install_constructor(vm, global, NativeFn::String, prototype);
    install_function(vm, constructor, NativeFn::StringFromCharCode);
}

pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    let string = match args.first() {
        None => JsString::empty(),
        Some(JsValue::Symbol(symbol)) if !construct => {
            JsString::from(vm.key_display(PropertyKey::Symbol(*symbol)).as_str())
        }
        Some(value) => vm.coerce_string(value)?,
    };
    if construct {
        let proto = vm.session.realm.string_prototype;
        Ok(JsValue::Ref(
            vm.new_class_object(proto, ObjectClass::BoxedString(string)),
        ))
    } else {
        vm.alloc_string(string)
    }
}

pub(super) fn from_char_code(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let mut units = Vec::with_capacity(args.len());
    for value in args {
        let n = vm.coerce_number(value)?;
        #[expect(clippy::cast_possible_truncation, reason = "fromCharCode keeps the low 16 bits")]
        units.push(to_uint32(n) as u16);
    }
    vm.alloc_string(JsString::from_units(&units))
}

/// The string a method was called on (`RequireObjectCoercible` + `ToString`).
fn this_string(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<JsString> {
    match this {
        JsValue::Undefined | JsValue::Null | JsValue::Hole => {
            Err(vm.type_error(&format!("String.prototype.{method} called on null or undefined")))
        }
        JsValue::Ref(id) => match vm.object(*id).map(|o| &o.class) {
            Some(ObjectClass::BoxedString(string)) => Ok(string.clone()),
            _ => vm.coerce_string(this),
        },
        other => vm.coerce_string(other),
    }
}

fn string_result(vm: &mut Vm<'_>, string: JsString) -> RunResult<JsValue> {
    vm.alloc_string(string)
}

pub(super) fn char_at(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "charAt")?;
    let index = integer_arg(vm, args, 0, 0.0)?;
    let unit = (index >= 0.0).then(|| string.unit_at(to_usize(index))).flatten();
    string_result(vm, unit.map_or_else(JsString::empty, |u| JsString::from_units(&[u])))
}

pub(super) fn char_code_at(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "charCodeAt")?;
    let index = integer_arg(vm, args, 0, 0.0)?;
    let unit = (index >= 0.0).then(|| string.unit_at(to_usize(index))).flatten();
    Ok(JsValue::Number(unit.map_or(f64::NAN, f64::from)))
}

pub(super) fn code_point_at(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "codePointAt")?;
    let index = integer_arg(vm, args, 0, 0.0)?;
    if index < 0.0 {
        return Ok(JsValue::Undefined);
    }
    let index = to_usize(index);
    Ok(match string.unit_at(index) {
        None => JsValue::Undefined,
        Some(high) if (0xD800..0xDC00).contains(&high) => match string.unit_at(index + 1) {
            Some(low) if (0xDC00..0xE000).contains(&low) => JsValue::Number(f64::from(
                0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00),
            )),
            _ => JsValue::Number(f64::from(high)),
        },
        Some(unit) => JsValue::Number(f64::from(unit)),
    })
}

/// The first index at or after `from` where `needle` occurs.
fn find_units(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return (from <= haystack.len()).then_some(from);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|i| &haystack[*i..*i + needle.len()] == needle)
}

/// The last index at or before `from` where `needle` occurs.
fn rfind_units(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    let last = (haystack.len() - needle.len()).min(from);
    (0..=last).rev().find(|i| &haystack[*i..*i + needle.len()] == needle)
}

pub(super) fn index_of(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "indexOf")?;
    let needle = vm.coerce_string(arg(args, 0))?;
    let from = to_usize(integer_arg(vm, args, 1, 0.0)?.min(string.len() as f64));
    let found = find_units(&string.to_units(), &needle.to_units(), from);
    Ok(JsValue::Number(found.map_or(-1.0, |i| i as f64)))
}

pub(super) fn last_index_of(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "lastIndexOf")?;
    let needle = vm.coerce_string(arg(args, 0))?;
    let position = match args.get(1) {
        None | Some(JsValue::Undefined) => f64::INFINITY,
        Some(value) => {
            let n = vm.coerce_number(value)?;
            if n.is_nan() { f64::INFINITY } else { n.trunc() }
        }
    };
    let from = to_usize(position.min(string.len() as f64));
    let found = rfind_units(&string.to_units(), &needle.to_units(), from);
    Ok(JsValue::Number(found.map_or(-1.0, |i| i as f64)))
}

pub(super) fn includes(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "includes")?;
    let needle = vm.coerce_string(arg(args, 0))?;
    let from = to_usize(integer_arg(vm, args, 1, 0.0)?.min(string.len() as f64));
    Ok(JsValue::Bool(
        find_units(&string.to_units(), &needle.to_units(), from).is_some(),
    ))
}

pub(super) fn starts_with(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "startsWith")?;
    let needle = vm.coerce_string(arg(args, 0))?.to_units();
    let from = to_usize(integer_arg(vm, args, 1, 0.0)?.min(string.len() as f64));
    let units = string.to_units();
    Ok(JsValue::Bool(units.get(from..from + needle.len()) == Some(&needle[..])))
}

pub(super) fn ends_with(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "endsWith")?;
    let needle = vm.coerce_string(arg(args, 0))?.to_units();
    let end = to_usize(integer_arg(vm, args, 1, string.len() as f64)?.min(string.len() as f64));
    let units = string.to_units();
    Ok(JsValue::Bool(
        end >= needle.len() && units[end - needle.len()..end] == needle[..],
    ))
}

pub(super) fn slice(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "slice")?;
    let length = string.len();
    let start = super::relative_index(integer_arg(vm, args, 0, 0.0)?, length);
    let end = super::relative_index(integer_arg(vm, args, 1, length as f64)?, length);
    string_result(vm, string.slice(start, end.max(start)))
}

pub(super) fn substring(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "substring")?;
    let length = string.len() as f64;
    let start = to_usize(integer_arg(vm, args, 0, 0.0)?.min(length));
    let end = to_usize(integer_arg(vm, args, 1, length)?.min(length));
    string_result(vm, string.slice(start.min(end), start.max(end)))
}

pub(super) fn to_upper_case(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "toUpperCase")?;
    let text = string.to_string_lossy().to_uppercase();
    string_result(vm, JsString::from(text.as_str()))
}

pub(super) fn to_lower_case(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "toLowerCase")?;
    let text = string.to_string_lossy().to_lowercase();
    string_result(vm, JsString::from(text.as_str()))
}

/// Trims JavaScript whitespace from the chosen ends.
fn trim_units(string: &JsString, start: bool, end: bool) -> JsString {
    let units = string.to_units();
    let is_space = |unit: &u16| char::from_u32(u32::from(*unit)).is_some_and(is_js_whitespace);
    let mut from = 0;
    let mut to = units.len();
    if start {
        while from < to && is_space(&units[from]) {
            from += 1;
        }
    }
    if end {
        while to > from && is_space(&units[to - 1]) {
            to -= 1;
        }
    }
    JsString::from_units(&units[from..to])
}

pub(super) fn trim(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "trim")?;
    string_result(vm, trim_units(&string, true, true))
}

pub(super) fn trim_start(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "trimStart")?;
    string_result(vm, trim_units(&string, true, false))
}

pub(super) fn trim_end(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "trimEnd")?;
    string_result(vm, trim_units(&string, false, true))
}

pub(super) fn split(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "split")?;
    let separator = arg(args, 0);
    if let Some(method) = protocol_method(vm, separator, WellKnownSymbol::Split)? {
        let limit = arg(args, 1).clone_with_heap(&vm.session.heap);
        return call_protocol(vm, method, separator, string, vec![limit]);
    }
    let limit = match args.get(1) {
        None | Some(JsValue::Undefined) => usize::MAX,
        Some(value) => {
            let n = vm.coerce_number(value)?;
            to_uint32(n) as usize
        }
    };
    let mut parts = Vec::new();
    if limit == 0 {
        return vm.new_array(parts);
    }
    let separator = match separator {
        JsValue::Undefined => {
            parts.push(vm.alloc_string(string)?);
            return vm.new_array(parts);
        }
        other => vm.coerce_string(other)?.to_units(),
    };
    let units = string.to_units();
    if separator.is_empty() {
        for unit in units.iter().take(limit) {
            parts.push(vm.alloc_string(JsString::from_units(&[*unit]))?);
        }
        return vm.new_array(parts);
    }
    let mut start = 0;
    while let Some(found) = find_units(&units, &separator, start) {
        if parts.len() >= limit {
            break;
        }
        parts.push(vm.alloc_string(JsString::from_units(&units[start..found]))?);
        start = found + separator.len();
        vm.tracker().check_memory_time_every(parts.len())?;
    }
    if parts.len() < limit {
        parts.push(vm.alloc_string(JsString::from_units(&units[start..]))?);
    }
    vm.new_array(parts)
}

pub(super) fn repeat(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "repeat")?;
    let count = integer_arg(vm, args, 0, 0.0)?;
    if count < 0.0 || count.is_infinite() {
        let shown = number_to_string(count);
        return Err(vm.range_error(&format!("Invalid count value: {shown}")));
    }
    let count = to_usize(count);
    vm.tracker()
        .check_allocation(string.len().saturating_mul(count).saturating_mul(2))?;
    let units = string.to_units();
    let mut out = Vec::with_capacity(units.len() * count);
    for _ in 0..count {
        out.extend_from_slice(&units);
    }
    string_result(vm, JsString::from_units(&out))
}

/// `padStart` / `padEnd`.
fn pad(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], at_start: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, if at_start { "padStart" } else { "padEnd" })?;
    let target = to_usize(integer_arg(vm, args, 0, 0.0)?);
    let filler = match arg(args, 1) {
        JsValue::Undefined => vec![u16::from(b' ')],
        other => vm.coerce_string(other)?.to_units(),
    };
    if target <= string.len() || filler.is_empty() {
        return string_result(vm, string);
    }
    vm.tracker().check_allocation(target * 2)?;
    let missing = target - string.len();
    let padding: Vec<u16> = filler.iter().copied().cycle().take(missing).collect();
    let units = string.to_units();
    let mut out = Vec::with_capacity(target);
    if at_start {
        out.extend(padding);
        out.extend(units);
    } else {
        out.extend(units);
        out.extend(padding);
    }
    string_result(vm, JsString::from_units(&out))
}

pub(super) fn pad_start(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    pad(vm, this, args, true)
}

pub(super) fn pad_end(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    pad(vm, this, args, false)
}

/// `replace` / `replaceAll`: a pattern with a `Symbol.replace` method (a
/// regexp) handles itself; otherwise a string pattern is matched literally
/// with a string or function replacement.
fn replace_impl(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], all: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, if all { "replaceAll" } else { "replace" })?;
    let pattern_value = arg(args, 0);
    let replacement = arg(args, 1);
    if !pattern_value.is_nullish() {
        if all && is_regexp(vm, pattern_value)? && !flags_include(vm, pattern_value, b'g', "replaceAll")? {
            return Err(vm.type_error("String.prototype.replaceAll called with a non-global RegExp argument"));
        }
        if let Some(method) = protocol_method(vm, pattern_value, WellKnownSymbol::Replace)? {
            let replacement = replacement.clone_with_heap(&vm.session.heap);
            return call_protocol(vm, method, pattern_value, string, vec![replacement]);
        }
    }
    let pattern = vm.coerce_string(pattern_value)?.to_units();
    let template = if vm.is_callable(replacement) {
        None
    } else {
        Some(vm.coerce_string(replacement)?.to_units())
    };
    let units = string.to_units();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut start = 0;
    let mut last_end = 0;
    while let Some(found) = find_units(&units, &pattern, start) {
        out.extend_from_slice(&units[last_end..found]);
        if let Some(template) = &template {
            let context = MatchContext {
                subject: &units,
                position: found,
                matched: &pattern,
                captures: &[],
                groups: None,
            };
            get_substitution(vm, template, &context, &mut out)?;
        } else {
            let matched_value = vm.alloc_string(JsString::from_units(&pattern))?;
            let whole = vm.alloc_string(string.clone())?;
            let result = vm.call(
                replacement.clone_with_heap(&vm.session.heap),
                JsValue::Undefined,
                vec![matched_value, JsValue::Number(found as f64), whole],
            )?;
            let text = vm.coerce_string(&result);
            result.drop_with(&mut vm.session.heap);
            out.extend(text?.units());
        }
        last_end = found + pattern.len();
        start = if pattern.is_empty() { found + 1 } else { last_end };
        vm.tracker().check_growth(out.len(), out.capacity(), 2)?;
        if !all || start > units.len() {
            break;
        }
    }
    if last_end <= units.len() {
        out.extend_from_slice(&units[last_end..]);
    }
    string_result(vm, JsString::from_units(&out))
}

/// The pieces `GetSubstitution` expands a replacement template against.
pub(super) struct MatchContext<'a> {
    pub subject: &'a [u16],
    /// Where the match starts in `subject`.
    pub position: usize,
    pub matched: &'a [u16],
    /// Captures 1..n; `None` for a group that did not participate.
    pub captures: &'a [Option<JsString>],
    /// The named-groups object, when the pattern has named groups.
    pub groups: Option<&'a JsValue>,
}

/// `GetSubstitution`: expands `$$`, `$&`, `` $` ``, `$'`, `$n`, `$nn` and
/// `$<name>` in `template` into `out`; anything else stays literal.
pub(super) fn get_substitution(
    vm: &mut Vm<'_>,
    template: &[u16],
    context: &MatchContext<'_>,
    out: &mut Vec<u16>,
) -> RunResult<()> {
    let dollar = u16::from(b'$');
    let digit = |unit: u16| {
        (u16::from(b'0')..=u16::from(b'9'))
            .contains(&unit)
            .then(|| usize::from(unit - u16::from(b'0')))
    };
    let group_count = context.captures.len();
    let mut i = 0;
    while i < template.len() {
        let unit = template[i];
        if unit != dollar || i + 1 >= template.len() {
            out.push(unit);
            i += 1;
            continue;
        }
        let next = template[i + 1];
        i += 2;
        let literal = |out: &mut Vec<u16>| {
            out.push(dollar);
            out.push(next);
        };
        if next == dollar {
            out.push(dollar);
        } else if next == u16::from(b'&') {
            out.extend_from_slice(context.matched);
        } else if next == u16::from(b'`') {
            out.extend_from_slice(&context.subject[..context.position]);
        } else if next == u16::from(b'\'') {
            let after = (context.position + context.matched.len()).min(context.subject.len());
            out.extend_from_slice(&context.subject[after..]);
        } else if let Some(first) = digit(next) {
            // `$nn` when that group exists, else `$n` and the second digit stays literal
            let two_digit = template
                .get(i)
                .and_then(|unit| digit(*unit))
                .map(|second| first * 10 + second)
                .filter(|n| (1..=group_count).contains(n));
            if let Some(n) = two_digit {
                push_capture(context, n, out);
                i += 1;
            } else if (1..=group_count).contains(&first) {
                push_capture(context, first, out);
            } else {
                literal(out);
            }
        } else if next == u16::from(b'<') {
            let close = context
                .groups
                .and_then(|_| template[i..].iter().position(|unit| *unit == u16::from(b'>')));
            match (context.groups, close) {
                (Some(groups), Some(close)) => {
                    let name = JsString::from_units(&template[i..i + close]);
                    i += close + 1;
                    let name_value = vm.alloc_string(name)?;
                    let key = vm.coerce_key(name_value)?;
                    let value = vm.get(groups, key)?;
                    if value != JsValue::Undefined {
                        let text = vm.coerce_string(&value);
                        value.drop_with(&mut vm.session.heap);
                        out.extend(text?.units());
                    }
                }
                _ => literal(out),
            }
        } else {
            literal(out);
        }
    }
    Ok(())
}

/// Appends capture `n` (1-based) when it participated.
fn push_capture(context: &MatchContext<'_>, n: usize, out: &mut Vec<u16>) {
    if let Some(Some(capture)) = context.captures.get(n - 1) {
        out.extend(capture.units());
    }
}

/// `GetMethod(value, symbol)` for an object argument: the protocol method a
/// regexp-like value supplies, `None` for a primitive or an absent method.
fn protocol_method(vm: &mut Vm<'_>, value: &JsValue, symbol: WellKnownSymbol) -> RunResult<Option<JsValue>> {
    if !value.is_object() {
        return Ok(None);
    }
    let method = vm.get(value, PropertyKey::Symbol(symbol.id()))?;
    if method.is_nullish() {
        method.drop_with(&mut vm.session.heap);
        Ok(None)
    } else if vm.is_callable(&method) {
        Ok(Some(method))
    } else {
        let shown = vm.inspect_short(&method);
        method.drop_with(&mut vm.session.heap);
        let receiver = vm.inspect_short(value)?;
        let key = vm.key_display(PropertyKey::Symbol(symbol.id()));
        Err(vm.type_error(&format!(
            "'{}' returned for property '{key}' of object '{receiver}' is not a function",
            shown?
        )))
    }
}

/// Calls a protocol method as `receiver[symbol](string, …extra)`, consuming `method` and `extra`.
fn call_protocol(
    vm: &mut Vm<'_>,
    method: JsValue,
    receiver: &JsValue,
    string: JsString,
    extra: Vec<JsValue>,
) -> RunResult<JsValue> {
    let string = match vm.alloc_string(string) {
        Ok(string) => string,
        Err(err) => {
            vm.drop_all(method, extra);
            return Err(err);
        }
    };
    let mut call_args = vec![string];
    call_args.extend(extra);
    vm.call(method, receiver.clone_with_heap(&vm.session.heap), call_args)
}

/// `IsRegExp`: an object whose `Symbol.match` is truthy, or a `RegExp` without one.
fn is_regexp(vm: &mut Vm<'_>, value: &JsValue) -> RunResult<bool> {
    let JsValue::Ref(id) = value else {
        return Ok(false);
    };
    let matcher = vm.get(value, PropertyKey::Symbol(WellKnownSymbol::Match.id()))?;
    let result = if matcher == JsValue::Undefined {
        matches!(vm.object(*id).map(|o| &o.class), Some(ObjectClass::RegExp(_)))
    } else {
        vm.to_boolean(&matcher)
    };
    matcher.drop_with(&mut vm.session.heap);
    Ok(result)
}

/// Whether `ToString(Get(regexp, "flags"))` contains `flag`.
fn flags_include(vm: &mut Vm<'_>, regexp: &JsValue, flag: u8, method: &str) -> RunResult<bool> {
    let key = key(vm, "flags");
    let flags = vm.get(regexp, key)?;
    if flags.is_nullish() {
        flags.drop_with(&mut vm.session.heap);
        return Err(vm.type_error(&format!(
            "The .flags property of the argument to String.prototype.{method} cannot be null or undefined"
        )));
    }
    let text = vm.coerce_string(&flags);
    flags.drop_with(&mut vm.session.heap);
    Ok(text?.units().any(|unit| unit == u16::from(flag)))
}

pub(super) fn match_(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "match")?;
    regexp_dispatch(
        vm,
        string,
        arg(args, 0),
        WellKnownSymbol::Match,
        None,
        regexp::symbol_match,
    )
}

pub(super) fn search(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "search")?;
    regexp_dispatch(
        vm,
        string,
        arg(args, 0),
        WellKnownSymbol::Search,
        None,
        regexp::symbol_search,
    )
}

pub(super) fn match_all(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "matchAll")?;
    let regexp_value = arg(args, 0);
    if !regexp_value.is_nullish() && is_regexp(vm, regexp_value)? && !flags_include(vm, regexp_value, b'g', "matchAll")?
    {
        return Err(vm.type_error("String.prototype.matchAll called with a non-global RegExp argument"));
    }
    regexp_dispatch(
        vm,
        string,
        regexp_value,
        WellKnownSymbol::MatchAll,
        Some("g"),
        regexp::symbol_match_all,
    )
}

/// `match` / `matchAll` / `search`: the argument's protocol method when it
/// has one, else `fallback` on a `RegExp` built from the argument (so
/// `undefined` matches everything).
fn regexp_dispatch(
    vm: &mut Vm<'_>,
    string: JsString,
    regexp_value: &JsValue,
    symbol: WellKnownSymbol,
    flags: Option<&str>,
    fallback: NativeImpl,
) -> RunResult<JsValue> {
    if !regexp_value.is_nullish()
        && let Some(method) = protocol_method(vm, regexp_value, symbol)?
    {
        return call_protocol(vm, method, regexp_value, string, Vec::new());
    }
    let flags = flags.map_or(JsValue::Undefined, |flags| vm.atom_value(flags));
    let ctor_args = [regexp_value.clone_with_heap(&vm.session.heap), flags];
    let rx = regexp::constructor(vm, &JsValue::Undefined, &ctor_args, true);
    for value in ctor_args {
        value.drop_with(&mut vm.session.heap);
    }
    let rx = rx?;
    let string = match vm.alloc_string(string) {
        Ok(string) => string,
        Err(err) => {
            rx.drop_with(&mut vm.session.heap);
            return Err(err);
        }
    };
    let result = fallback(vm, &rx, slice::from_ref(&string), false);
    vm.drop_all(rx, vec![string]);
    result
}

pub(super) fn replace(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    replace_impl(vm, this, args, false)
}

pub(super) fn replace_all(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    replace_impl(vm, this, args, true)
}

/// `normalize(form)` through the vendored Unicode 17 tables; `form` defaults to NFC.
pub(super) fn normalize(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "normalize")?;
    let form = match arg(args, 0) {
        JsValue::Undefined => NormalizationForm::Nfc,
        other => match vm.coerce_string(other)?.to_string_lossy().as_str() {
            "NFC" => NormalizationForm::Nfc,
            "NFD" => NormalizationForm::Nfd,
            "NFKC" => NormalizationForm::Nfkc,
            "NFKD" => NormalizationForm::Nfkd,
            _ => {
                return Err(vm.range_error("The normalization form should be one of NFC, NFD, NFKC, NFKD."));
            }
        },
    };
    // a decomposition can grow the string several times over
    vm.tracker().check_allocation(string.len().saturating_mul(8))?;
    let normalized = match &string {
        JsString::Latin1(bytes) => unicode::normalize::normalize(&**bytes, form),
        JsString::Utf16(units) => unicode::normalize::normalize(&**units, form),
    };
    match normalized {
        Ok(buffer) => string_result(vm, JsString::from_units(&buffer.to_utf16())),
        Err(_) => Err(vm.range_error("Invalid string length")),
    }
}

pub(super) fn at(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "at")?;
    let length = string.len() as f64;
    let index = integer_arg(vm, args, 0, 0.0)?;
    let index = if index < 0.0 { length + index } else { index };
    if index < 0.0 || index >= length {
        return Ok(JsValue::Undefined);
    }
    let unit = string.unit_at(to_usize(index)).expect("in range");
    string_result(vm, JsString::from_units(&[unit]))
}

pub(super) fn concat(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "concat")?;
    let mut parts = vec![vm.alloc_string(string)?];
    parts.extend(args.iter().map(|a| a.clone_with_heap(&vm.session.heap)));
    vm.concat_strings(parts)
}

pub(super) fn to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    match this {
        JsValue::Atom(_) | JsValue::Str(_) => Ok(this.clone_with_heap(&vm.session.heap)),
        JsValue::Ref(id) => match vm.object(*id).map(|o| &o.class) {
            Some(ObjectClass::BoxedString(string)) => {
                let string = string.clone();
                vm.alloc_string(string)
            }
            _ => Err(vm.type_error("String.prototype.toString requires that 'this' be a String")),
        },
        _ => Err(vm.type_error("String.prototype.toString requires that 'this' be a String")),
    }
}

/// `localeCompare` without locales: code-unit order.
pub(super) fn locale_compare(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "localeCompare")?;
    let other = vm.coerce_string(arg(args, 0))?;
    Ok(JsValue::Number(match string.cmp(&other) {
        Ordering::Less => -1.0,
        Ordering::Equal => 0.0,
        Ordering::Greater => 1.0,
    }))
}

pub(super) fn iterator(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let string = this_string(vm, this, "[Symbol.iterator]")?;
    let value = vm.alloc_string(string)?;
    vm.get_iterator(value)
}
