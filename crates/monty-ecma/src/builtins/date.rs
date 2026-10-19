//! `Date`: a time value in milliseconds since the epoch (`NaN` when invalid),
//! read through the session's `OsPolicy` clock and shown in its zone.
//!
//! Calendar arithmetic follows the specification's `MakeDay`/`MakeTime`
//! algorithms in `f64`, so out-of-range components overflow into the next
//! unit as they do in Node; chrono only splits a valid instant into fields.

use chrono::{DateTime, NaiveDate};
use monty_types::{DateTimeSource, MontyDateTime, OsFunctionCall, SandboxTimeZone, TimeCaller};

use super::{NativeFn, arg, install_constructor, install_function, install_symbol_function, raw_object};
use crate::{
    error::RunResult,
    heap::{HeapId, ObjectClass},
    intern::WellKnownSymbol,
    string::JsString,
    value::JsValue,
    vm::{Vm, host::OsAnswer, ops::Hint},
};

const MS_PER_DAY: f64 = 86_400_000.0;
/// `TimeClip`'s bound: ±100 million days around the epoch.
const MAX_TIME: f64 = 8.64e15;
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    let prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.date_prototype = prototype;
    for native in [
        NativeFn::DateProtoGetTime,
        NativeFn::DateProtoValueOf,
        NativeFn::DateProtoGetTimezoneOffset,
        NativeFn::DateProtoGetFullYear,
        NativeFn::DateProtoGetMonth,
        NativeFn::DateProtoGetDate,
        NativeFn::DateProtoGetDay,
        NativeFn::DateProtoGetHours,
        NativeFn::DateProtoGetMinutes,
        NativeFn::DateProtoGetSeconds,
        NativeFn::DateProtoGetMilliseconds,
        NativeFn::DateProtoGetUtcFullYear,
        NativeFn::DateProtoGetUtcMonth,
        NativeFn::DateProtoGetUtcDate,
        NativeFn::DateProtoGetUtcDay,
        NativeFn::DateProtoGetUtcHours,
        NativeFn::DateProtoGetUtcMinutes,
        NativeFn::DateProtoGetUtcSeconds,
        NativeFn::DateProtoGetUtcMilliseconds,
        NativeFn::DateProtoSetTime,
        NativeFn::DateProtoSetMilliseconds,
        NativeFn::DateProtoSetSeconds,
        NativeFn::DateProtoSetMinutes,
        NativeFn::DateProtoSetHours,
        NativeFn::DateProtoSetDate,
        NativeFn::DateProtoSetMonth,
        NativeFn::DateProtoSetFullYear,
        NativeFn::DateProtoSetUtcMilliseconds,
        NativeFn::DateProtoSetUtcSeconds,
        NativeFn::DateProtoSetUtcMinutes,
        NativeFn::DateProtoSetUtcHours,
        NativeFn::DateProtoSetUtcDate,
        NativeFn::DateProtoSetUtcMonth,
        NativeFn::DateProtoSetUtcFullYear,
        NativeFn::DateProtoToIsoString,
        NativeFn::DateProtoToJson,
        NativeFn::DateProtoToString,
        NativeFn::DateProtoToDateString,
        NativeFn::DateProtoToTimeString,
        NativeFn::DateProtoToUtcString,
        NativeFn::DateProtoToLocaleString,
        NativeFn::DateProtoToLocaleDateString,
        NativeFn::DateProtoToLocaleTimeString,
    ] {
        install_function(vm, prototype, native);
    }
    install_symbol_function(
        vm,
        prototype,
        WellKnownSymbol::ToPrimitive,
        NativeFn::DateProtoToPrimitive,
    );
    let constructor = install_constructor(vm, global, NativeFn::Date, prototype);
    for native in [NativeFn::DateNow, NativeFn::DateParse, NativeFn::DateUtc] {
        install_function(vm, constructor, native);
    }
}

// === Time values ===

/// `TimeClip`: `NaN` outside the representable range, else the integer value.
fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > MAX_TIME {
        f64::NAN
    } else {
        t.trunc() + 0.0
    }
}

/// Days from the epoch to the first of `month` (0-based, any integer) in `year`.
fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let y = year + (month / 12.0).floor();
    let m = month.rem_euclid(12.0);
    if y.abs() > 400_000.0 {
        return f64::NAN;
    }
    // Howard Hinnant's days-from-civil, for the first of the month
    #[expect(clippy::cast_possible_truncation, reason = "the year is bounded above")]
    let (y, m) = (y as i64, m as i64 + 1);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    #[expect(clippy::cast_precision_loss, reason = "day counts are far below 2^53")]
    let days = (era * 146_097 + doe - 719_468) as f64;
    days + date - 1.0
}

fn make_time(hour: f64, minute: f64, second: f64, ms: f64) -> f64 {
    if [hour, minute, second, ms].iter().any(|v| !v.is_finite()) {
        return f64::NAN;
    }
    hour.trunc() * 3_600_000.0 + minute.trunc() * 60_000.0 + second.trunc() * 1000.0 + ms.trunc()
}

fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        f64::NAN
    } else {
        day * MS_PER_DAY + time
    }
}

/// The calendar fields of a (finite) time value.
#[derive(Clone, Copy)]
struct Fields {
    year: i32,
    /// 0-based.
    month: u32,
    day: u32,
    weekday: u32,
    hour: u32,
    minute: u32,
    second: u32,
    ms: u32,
}

/// Splits a time value into fields (Howard Hinnant's civil-from-days, so
/// the whole ±8.64e15 range works); `None` when it is invalid.
fn fields(t: f64) -> Option<Fields> {
    if t.is_nan() {
        return None;
    }
    #[expect(clippy::cast_possible_truncation, reason = "a clipped time value fits i64")]
    let ms = t as i64;
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "calendar fields are small and non-negative after the euclidean splits"
    )]
    Some(Fields {
        year: year as i32,
        month: (month - 1) as u32,
        day: day as u32,
        weekday: (days + 4).rem_euclid(7) as u32,
        hour: (in_day / 3_600_000) as u32,
        minute: (in_day / 60_000 % 60) as u32,
        second: (in_day / 1000 % 60) as u32,
        ms: (in_day % 1000) as u32,
    })
}

/// The zone offset (ms) in force at UTC time value `t`.
fn offset_at(zone: &SandboxTimeZone, t: f64) -> f64 {
    #[expect(clippy::cast_possible_truncation, reason = "a clipped time value fits i64")]
    let naive = DateTime::from_timestamp_millis(t as i64).map(|d| d.naive_utc());
    naive.map_or(0.0, |naive| f64::from(zone.at(naive).offset_seconds) * 1000.0)
}

/// `LocalTime(t)`.
fn local_time(zone: &SandboxTimeZone, t: f64) -> f64 {
    t + offset_at(zone, t)
}

/// `UTC(local)`: the instant whose wall-clock reading in the zone is `local`.
fn utc_from_local(zone: &SandboxTimeZone, local: f64) -> f64 {
    if local.is_nan() {
        return f64::NAN;
    }
    #[expect(clippy::cast_possible_truncation, reason = "a clipped time value fits i64")]
    let naive = DateTime::from_timestamp_millis(local as i64).map(|d| d.naive_utc());
    let offset = naive
        .and_then(|naive| zone.offset_for_local(naive))
        .map_or_else(|| offset_at(zone, local), |seconds| f64::from(seconds) * 1000.0);
    local - offset
}

fn zone_name(zone: &SandboxTimeZone, t: f64) -> String {
    #[expect(clippy::cast_possible_truncation, reason = "a clipped time value fits i64")]
    let naive = DateTime::from_timestamp_millis(t as i64).map(|d| d.naive_utc());
    let name = naive.and_then(|naive| zone.at(naive).name);
    match name.as_deref() {
        None | Some("UTC") => "Coordinated Universal Time".to_owned(),
        Some(name) => name.to_owned(),
    }
}

// === Formatting ===

fn pad2(value: u32) -> String {
    format!("{value:02}")
}

/// `toISOString`'s shape: expanded six-digit years outside 0..=9999.
fn iso_string(t: f64) -> Option<String> {
    let f = fields(t)?;
    let year = if (0..=9999).contains(&f.year) {
        format!("{:04}", f.year)
    } else {
        format!("{}{:06}", if f.year < 0 { '-' } else { '+' }, f.year.abs())
    };
    Some(format!(
        "{year}-{}-{}T{}:{}:{}.{:03}Z",
        pad2(f.month + 1),
        pad2(f.day),
        pad2(f.hour),
        pad2(f.minute),
        pad2(f.second),
        f.ms
    ))
}

/// `Mon Oct 19 2026`.
fn date_part(f: &Fields) -> String {
    let year = if f.year < 0 {
        format!("-{:06}", -f.year)
    } else {
        format!("{:04}", f.year)
    };
    format!(
        "{} {} {} {year}",
        WEEKDAYS[f.weekday as usize],
        MONTHS[f.month as usize],
        pad2(f.day)
    )
}

/// `22:16:11 GMT+0000 (Coordinated Universal Time)`.
fn time_part(f: &Fields, offset_ms: f64, zone_name: &str) -> String {
    #[expect(clippy::cast_possible_truncation, reason = "offsets are a few hours")]
    let minutes = (offset_ms / 60_000.0).round() as i64;
    let sign = if minutes < 0 { '-' } else { '+' };
    format!(
        "{}:{}:{} GMT{sign}{:02}{:02} ({zone_name})",
        pad2(f.hour),
        pad2(f.minute),
        pad2(f.second),
        minutes.abs() / 60,
        minutes.abs() % 60
    )
}

/// `Mon, 19 Oct 2026 22:16:11 GMT`.
fn utc_string(t: f64) -> Option<String> {
    let f = fields(t)?;
    Some(format!(
        "{}, {} {} {:04} {}:{}:{} GMT",
        WEEKDAYS[f.weekday as usize],
        pad2(f.day),
        MONTHS[f.month as usize],
        f.year,
        pad2(f.hour),
        pad2(f.minute),
        pad2(f.second)
    ))
}

/// The en-US `toLocale*` shapes: `10/19/2026`, `10:16:11 PM`.
fn locale_date(f: &Fields) -> String {
    format!("{}/{}/{}", f.month + 1, f.day, f.year)
}

fn locale_time(f: &Fields) -> String {
    let hour12 = match f.hour % 12 {
        0 => 12,
        other => other,
    };
    let suffix = if f.hour < 12 { "AM" } else { "PM" };
    format!("{hour12}:{}:{} {suffix}", pad2(f.minute), pad2(f.second))
}

// === Parsing ===

/// `Date.parse`: the ISO format (date-only as UTC, date-time without an
/// offset as local), then the `toString` / `toUTCString` shapes.
fn parse(zone: &SandboxTimeZone, text: &str) -> f64 {
    let text = text.trim();
    parse_iso(zone, text)
        .or_else(|| parse_textual(zone, text))
        .map_or(f64::NAN, time_clip)
}

fn parse_iso(zone: &SandboxTimeZone, text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    // the year: four digits, or a sign and six
    let (year, mut pos): (f64, usize) = match bytes.first()? {
        b'+' | b'-' => {
            let negative = bytes[0] == b'-';
            let digits = digits(bytes, 1, 6)?;
            if negative && digits == 0 {
                return None;
            }
            (
                if negative {
                    -f64::from(digits)
                } else {
                    f64::from(digits)
                },
                7,
            )
        }
        _ => (f64::from(digits(bytes, 0, 4)?), 4),
    };
    let mut month = 1.0;
    let mut day = 1.0;
    if bytes.get(pos) == Some(&b'-') {
        month = f64::from(digits(bytes, pos + 1, 2)?);
        pos += 3;
        if bytes.get(pos) == Some(&b'-') {
            day = f64::from(digits(bytes, pos + 1, 2)?);
            pos += 3;
        }
    }
    if !(1.0..=12.0).contains(&month) || !(1.0..=31.0).contains(&day) {
        return None;
    }
    let date_only = pos == bytes.len();
    let (mut hour, mut minute, mut second, mut ms) = (0.0, 0.0, 0.0, 0.0);
    let mut offset: Option<f64> = None;
    if !date_only {
        if bytes.get(pos) != Some(&b'T') && bytes.get(pos) != Some(&b't') && bytes.get(pos) != Some(&b' ') {
            return None;
        }
        hour = f64::from(digits(bytes, pos + 1, 2)?);
        if bytes.get(pos + 3) != Some(&b':') {
            return None;
        }
        minute = f64::from(digits(bytes, pos + 4, 2)?);
        pos += 6;
        if bytes.get(pos) == Some(&b':') {
            second = f64::from(digits(bytes, pos + 1, 2)?);
            pos += 3;
            if bytes.get(pos) == Some(&b'.') || bytes.get(pos) == Some(&b',') {
                let start = pos + 1;
                let mut end = start;
                while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                }
                if end == start {
                    return None;
                }
                let fraction = &text[start..end];
                let scaled: f64 = fraction.chars().take(3).collect::<String>().parse().ok()?;
                let used = i32::try_from(fraction.len().min(3)).unwrap_or(3);
                ms = scaled * 10f64.powi(3 - used);
                pos = end;
            }
        }
        if hour > 24.0 || minute > 59.0 || second > 59.0 || (hour >= 24.0 && (minute > 0.0 || second > 0.0 || ms > 0.0))
        {
            return None;
        }
        match bytes.get(pos) {
            None => {}
            Some(b'Z' | b'z') if pos + 1 == bytes.len() => offset = Some(0.0),
            Some(b'+' | b'-') => {
                let sign = if bytes[pos] == b'-' { -1.0 } else { 1.0 };
                let hours = f64::from(digits(bytes, pos + 1, 2)?);
                let minutes = match bytes.get(pos + 3) {
                    Some(b':') => {
                        let minutes = f64::from(digits(bytes, pos + 4, 2)?);
                        if pos + 6 != bytes.len() {
                            return None;
                        }
                        minutes
                    }
                    Some(_) => {
                        let minutes = f64::from(digits(bytes, pos + 3, 2)?);
                        if pos + 5 != bytes.len() {
                            return None;
                        }
                        minutes
                    }
                    None => {
                        if pos + 3 != bytes.len() {
                            return None;
                        }
                        0.0
                    }
                };
                offset = Some(sign * (hours * 60.0 + minutes) * 60_000.0);
            }
            Some(_) => return None,
        }
    }
    let local = make_date(make_day(year, month - 1.0, day), make_time(hour, minute, second, ms));
    Some(match offset {
        Some(offset) => local - offset,
        None if date_only => local,
        None => utc_from_local(zone, local),
    })
}

/// Exactly `count` ASCII digits at `start`.
fn digits(bytes: &[u8], start: usize, count: usize) -> Option<u32> {
    let slice = bytes.get(start..start + count)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    slice
        .iter()
        .try_fold(0u32, |acc, b| acc.checked_mul(10)?.checked_add(u32::from(b - b'0')))
}

/// `Mon Oct 19 2026 22:16:11 GMT+0000 (zone)` and `Mon, 19 Oct 2026 22:16:11 GMT`:
/// an optional weekday, then month and day in either order, year, an optional
/// time, an optional `GMT`/`UTC`/`Z` with an optional `±hhmm` offset.
fn parse_textual(zone: &SandboxTimeZone, text: &str) -> Option<f64> {
    let text = match text.find('(') {
        Some(index) => &text[..index],
        None => text,
    };
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    let mut month: Option<f64> = None;
    let mut day: Option<f64> = None;
    let mut year: Option<f64> = None;
    let mut time = 0.0;
    let mut offset: Option<f64> = None;
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        if let Some(index) = MONTHS.iter().position(|m| lower.starts_with(&m.to_ascii_lowercase())) {
            month = Some(index as f64);
        } else if WEEKDAYS.iter().any(|d| lower.starts_with(&d.to_ascii_lowercase())) {
            // the weekday is informative only
        } else if token.contains(':') {
            let parts: Vec<&str> = token.split(':').collect();
            if parts.len() < 2 || parts.len() > 3 {
                return None;
            }
            let hour: f64 = parts[0].parse::<u32>().ok()?.into();
            let minute: f64 = parts[1].parse::<u32>().ok()?.into();
            let second: f64 = parts.get(2).map_or(Some(0), |s| s.parse::<u32>().ok())?.into();
            time = make_time(hour, minute, second, 0.0);
        } else if let Some(rest) = lower
            .strip_prefix("gmt")
            .or_else(|| lower.strip_prefix("utc"))
            .or_else(|| lower.strip_prefix('z'))
        {
            offset = Some(parse_offset(rest)?);
        } else if let Some(stripped) = token.strip_prefix('+').or_else(|| token.strip_prefix('-'))
            && stripped.len() == 4
            && stripped.bytes().all(|b| b.is_ascii_digit())
        {
            offset = Some(parse_offset(token)?);
        } else if let Ok(number) = token.parse::<u32>() {
            if day.is_none() && number <= 31 && (month.is_some() || year.is_none()) {
                day = Some(f64::from(number));
            } else {
                year = Some(f64::from(number));
            }
        } else {
            return None;
        }
    }
    let (Some(month), Some(day), Some(year)) = (month, day, year) else {
        return None;
    };
    let year = if year < 50.0 {
        year + 2000.0
    } else if year < 100.0 {
        year + 1900.0
    } else {
        year
    };
    let local = make_date(make_day(year, month, day), time);
    Some(match offset {
        Some(offset) => local - offset,
        None => utc_from_local(zone, local),
    })
}

/// `±hhmm`, `±hh:mm` or nothing, as milliseconds.
fn parse_offset(text: &str) -> Option<f64> {
    if text.is_empty() {
        return Some(0.0);
    }
    let sign = match text.as_bytes()[0] {
        b'+' => 1.0,
        b'-' => -1.0,
        _ => return None,
    };
    let digits: String = text[1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: f64 = digits[..2].parse::<u32>().ok()?.into();
    let minutes: f64 = digits[2..].parse::<u32>().ok()?.into();
    Some(sign * (hours * 60.0 + minutes) * 60_000.0)
}

// === Objects ===

/// Allocates a `Date` holding `t` (one owned reference).
pub(crate) fn new_date(vm: &mut Vm<'_>, t: f64) -> JsValue {
    let proto = vm.session.realm.date_prototype;
    JsValue::Ref(vm.new_class_object(proto, ObjectClass::Date(time_clip(t))))
}

/// The time value of the date a method was called on.
fn this_time(vm: &mut Vm<'_>, this: &JsValue) -> RunResult<(HeapId, f64)> {
    if let JsValue::Ref(id) = this
        && let Some(ObjectClass::Date(t)) = vm.object(*id).map(|o| &o.class)
    {
        return Ok((*id, *t));
    }
    Err(vm.type_error("this is not a Date object."))
}

fn set_time_value(vm: &mut Vm<'_>, id: HeapId, t: f64) -> JsValue {
    let t = time_clip(t);
    if let Some(ObjectClass::Date(slot)) = vm.object_mut(id).map(|o| &mut o.class) {
        *slot = t;
    }
    JsValue::Number(t)
}

/// The current instant in milliseconds, or the OS call that reads it when
/// the policy asks the host.
fn now(vm: &mut Vm<'_>, answer: OsAnswer) -> RunResult<f64> {
    match vm.session.os_policy.datetime {
        DateTimeSource::CallHost => Err(vm.os_call(OsFunctionCall::Time(TimeCaller::Time), answer)),
        source => {
            let instant = source.read().unwrap_or_else(|| DateTime::UNIX_EPOCH.naive_utc());
            #[expect(
                clippy::cast_precision_loss,
                reason = "milliseconds since the epoch are far below 2^53"
            )]
            Ok(instant.and_utc().timestamp_millis() as f64)
        }
    }
}

/// `new Date(...)` / `Date()`.
pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    if !construct {
        let t = now(vm, OsAnswer::DateString)?;
        let text = to_string_value(vm, t);
        return vm.alloc_string(text);
    }
    let t = match args.len() {
        0 => now(vm, OsAnswer::NewDate)?,
        1 => {
            let value = &args[0];
            if let JsValue::Ref(id) = value
                && let Some(ObjectClass::Date(t)) = vm.object(*id).map(|o| &o.class)
            {
                *t
            } else {
                let primitive = vm.coerce_primitive(value, Hint::Default)?;
                let t = if primitive.is_string() {
                    let text = vm.string_of(&primitive).expect("string").to_string_lossy();
                    parse(&vm.session.os_policy.timezone, &text)
                } else {
                    vm.coerce_number(&primitive)?
                };
                primitive.drop_with(&mut vm.session.heap);
                t
            }
        }
        _ => {
            let local = components(vm, args)?;
            utc_from_local(&vm.session.os_policy.timezone, local)
        }
    };
    Ok(new_date(vm, t))
}

/// A time value from `year, month[, day, hours, minutes, seconds, ms]`
/// (two-digit years are 19xx), before the zone is applied.
fn components(vm: &mut Vm<'_>, args: &[JsValue]) -> RunResult<f64> {
    let mut values = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
    for (index, slot) in values.iter_mut().enumerate() {
        if let Some(value) = args.get(index) {
            *slot = vm.coerce_number(value)?;
        }
    }
    let year = values[0].trunc();
    let year = if (0.0..=99.0).contains(&year) {
        1900.0 + year
    } else {
        year
    };
    let [_, month, day, hour, minute, second, ms] = values;
    Ok(make_date(
        make_day(year, month.trunc(), day.trunc()),
        make_time(hour, minute, second, ms),
    ))
}

pub(super) fn now_static(vm: &mut Vm<'_>, _: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    now(vm, OsAnswer::ClockMillis).map(JsValue::Number)
}

pub(super) fn parse_static(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let text = vm.coerce_string(arg(args, 0))?.to_string_lossy();
    Ok(JsValue::Number(parse(&vm.session.os_policy.timezone, &text)))
}

pub(super) fn utc(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let t = components(vm, args)?;
    Ok(JsValue::Number(time_clip(t)))
}

/// Turns a host clock answer (seconds since the epoch) into what the
/// suspended call wanted.
pub(crate) fn from_host_seconds(vm: &mut Vm<'_>, seconds: f64, answer: OsAnswer) -> RunResult<JsValue> {
    let t = (seconds * 1000.0).floor();
    Ok(match answer {
        OsAnswer::ClockMillis => JsValue::Number(t),
        OsAnswer::NewDate => new_date(vm, t),
        OsAnswer::DateString => {
            let text = to_string_value(vm, t);
            vm.alloc_string(text)?
        }
    })
}

// === Getters ===

/// A getter over the time value's fields, local or UTC.
fn field(vm: &mut Vm<'_>, this: &JsValue, local: bool, pick: fn(&Fields) -> u32) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    let t = if local {
        local_time(&vm.session.os_policy.timezone, t)
    } else {
        t
    };
    Ok(JsValue::Number(fields(t).map_or(f64::NAN, |f| f64::from(pick(&f)))))
}

pub(super) fn get_time(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    this_time(vm, this).map(|(_, t)| JsValue::Number(t))
}

pub(super) fn get_timezone_offset(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    if t.is_nan() {
        return Ok(JsValue::Number(f64::NAN));
    }
    Ok(JsValue::Number(
        (0.0 - offset_at(&vm.session.os_policy.timezone, t)) / 60_000.0,
    ))
}

pub(super) fn get_full_year(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    let t = local_time(&vm.session.os_policy.timezone, t);
    Ok(JsValue::Number(fields(t).map_or(f64::NAN, |f| f64::from(f.year))))
}

pub(super) fn get_utc_full_year(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    Ok(JsValue::Number(fields(t).map_or(f64::NAN, |f| f64::from(f.year))))
}

macro_rules! getters {
    ($( $local:ident / $utc:ident => $field:ident ),* $(,)?) => {
        $(
            pub(super) fn $local(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
                field(vm, this, true, |f| f.$field)
            }

            pub(super) fn $utc(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
                field(vm, this, false, |f| f.$field)
            }
        )*
    };
}

getters! {
    get_month / get_utc_month => month,
    get_date / get_utc_date => day,
    get_day / get_utc_day => weekday,
    get_hours / get_utc_hours => hour,
    get_minutes / get_utc_minutes => minute,
    get_seconds / get_utc_seconds => second,
    get_milliseconds / get_utc_milliseconds => ms,
}

// === Setters ===

pub(super) fn set_time(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (id, _) = this_time(vm, this)?;
    let t = vm.coerce_number(arg(args, 0))?;
    Ok(set_time_value(vm, id, t))
}

/// Which field a setter starts at; the following fields may be given too.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unit {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Ms,
}

/// The shared setter: decomposes the time (local or UTC), replaces the
/// fields from `unit` on with the arguments given, and recomposes.
fn set_fields(
    vm: &mut Vm<'_>,
    this: &JsValue,
    args: &[JsValue],
    local: bool,
    unit: Unit,
    max: usize,
) -> RunResult<JsValue> {
    let (id, t) = this_time(vm, this)?;
    let zone = vm.session.os_policy.timezone.clone();
    let base = if local { local_time(&zone, t) } else { t };
    // a year setter on an invalid date starts from +0, the others keep NaN
    let f = fields(if base.is_nan() && unit == Unit::Year { 0.0 } else { base });
    let mut parts = f.map_or([f64::NAN; 7], |f| {
        [
            f64::from(f.year),
            f64::from(f.month),
            f64::from(f.day),
            f64::from(f.hour),
            f64::from(f.minute),
            f64::from(f.second),
            f64::from(f.ms),
        ]
    });
    let first = unit as usize;
    let mut given = Vec::with_capacity(max);
    for (offset, value) in args.iter().take(max).enumerate() {
        given.push((first + offset, vm.coerce_number(value)?));
    }
    if given.is_empty() {
        given.push((first, f64::NAN));
    }
    for (index, value) in given {
        parts[index] = value;
    }
    let [year, month, day, hour, minute, second, ms] = parts;
    let composed = make_date(make_day(year, month, day), make_time(hour, minute, second, ms));
    let t = if local {
        utc_from_local(&zone, composed)
    } else {
        composed
    };
    Ok(set_time_value(vm, id, t))
}

macro_rules! setters {
    ($( $local:ident / $utc:ident => $unit:ident, $max:expr ),* $(,)?) => {
        $(
            pub(super) fn $local(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
                set_fields(vm, this, args, true, Unit::$unit, $max)
            }

            pub(super) fn $utc(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
                set_fields(vm, this, args, false, Unit::$unit, $max)
            }
        )*
    };
}

setters! {
    set_full_year / set_utc_full_year => Year, 3,
    set_month / set_utc_month => Month, 2,
    set_date / set_utc_date => Day, 1,
    set_hours / set_utc_hours => Hour, 4,
    set_minutes / set_utc_minutes => Minute, 3,
    set_seconds / set_utc_seconds => Second, 2,
    set_milliseconds / set_utc_milliseconds => Ms, 1,
}

// === Strings ===

/// `toString`'s text, `Invalid Date` for `NaN`.
pub(crate) fn to_string_value(vm: &Vm<'_>, t: f64) -> JsString {
    let zone = &vm.session.os_policy.timezone;
    let text = fields(local_time(zone, t)).map_or_else(
        || "Invalid Date".to_owned(),
        |f| {
            format!(
                "{} {}",
                date_part(&f),
                time_part(&f, offset_at(zone, t), &zone_name(zone, t))
            )
        },
    );
    JsString::from(text.as_str())
}

fn text_result(vm: &mut Vm<'_>, text: &str) -> RunResult<JsValue> {
    vm.alloc_string(JsString::from(text))
}

pub(super) fn to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    let text = to_string_value(vm, t);
    vm.alloc_string(text)
}

pub(super) fn to_date_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    let local = local_time(&vm.session.os_policy.timezone, t);
    text_result(
        vm,
        &fields(local).map_or_else(|| "Invalid Date".to_owned(), |f| date_part(&f)),
    )
}

pub(super) fn to_time_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    let zone = &vm.session.os_policy.timezone;
    let text = fields(local_time(zone, t)).map_or_else(
        || "Invalid Date".to_owned(),
        |f| time_part(&f, offset_at(zone, t), &zone_name(zone, t)),
    );
    text_result(vm, &text)
}

pub(super) fn to_utc_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    text_result(vm, &utc_string(t).unwrap_or_else(|| "Invalid Date".to_owned()))
}

pub(super) fn to_iso_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    match iso_string(t) {
        Some(text) => text_result(vm, &text),
        None => Err(vm.range_error("Invalid time value")),
    }
}

/// `toJSON`: `null` for an invalid date, else `toISOString`.
pub(super) fn to_json(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    match iso_string(t) {
        Some(text) => text_result(vm, &text),
        None => Ok(JsValue::Null),
    }
}

/// `toLocaleString` and friends in the en-US shape.
fn locale(vm: &mut Vm<'_>, this: &JsValue, date: bool, time: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    let local = local_time(&vm.session.os_policy.timezone, t);
    let text = match fields(local) {
        None => "Invalid Date".to_owned(),
        Some(f) => match (date, time) {
            (true, true) => format!("{}, {}", locale_date(&f), locale_time(&f)),
            (true, false) => locale_date(&f),
            _ => locale_time(&f),
        },
    };
    text_result(vm, &text)
}

pub(super) fn to_locale_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    locale(vm, this, true, true)
}

pub(super) fn to_locale_date_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    locale(vm, this, true, false)
}

pub(super) fn to_locale_time_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    locale(vm, this, false, true)
}

pub(super) fn value_of(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    this_time(vm, this).map(|(_, t)| JsValue::Number(t))
}

/// `[Symbol.toPrimitive](hint)`: `"number"` prefers `valueOf`, the others `toString`.
pub(super) fn to_primitive(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let (_, t) = this_time(vm, this)?;
    let hint = arg(args, 0);
    let text = hint
        .is_string()
        .then(|| vm.string_of(hint).expect("string").to_string_lossy());
    match text.as_deref() {
        Some("number") => Ok(JsValue::Number(t)),
        Some("string" | "default") => {
            let text = to_string_value(vm, t);
            vm.alloc_string(text)
        }
        _ => {
            let shown = match hint {
                JsValue::Atom(_) | JsValue::Str(_) => text.unwrap_or_default(),
                other => vm.inspect_short(other)?,
            };
            Err(vm.type_error(&format!("Invalid hint: {shown}")))
        }
    }
}

/// A `MontyDateTime` as a time value (a naive value is read as UTC).
pub(crate) fn time_from_host(value: &MontyDateTime) -> f64 {
    let naive = NaiveDate::from_ymd_opt(value.year, u32::from(value.month), u32::from(value.day)).and_then(|d| {
        d.and_hms_micro_opt(
            u32::from(value.hour),
            u32::from(value.minute),
            u32::from(value.second),
            value.microsecond,
        )
    });
    naive.map_or(f64::NAN, |naive| {
        #[expect(
            clippy::cast_precision_loss,
            reason = "milliseconds since the epoch are far below 2^53"
        )]
        let ms = naive.and_utc().timestamp_millis() as f64;
        ms - f64::from(value.offset_seconds.unwrap_or(0)) * 1000.0
    })
}

/// A time value as a UTC `MontyDateTime`, `None` for an invalid date or one
/// outside the host's calendar.
pub(crate) fn time_to_host(t: f64) -> Option<MontyDateTime> {
    let f = fields(t)?;
    if !(1..=9999).contains(&f.year) {
        return None;
    }
    #[expect(clippy::cast_possible_truncation, reason = "calendar fields are small")]
    Some(MontyDateTime {
        year: f.year,
        month: (f.month + 1) as u8,
        day: f.day as u8,
        hour: f.hour as u8,
        minute: f.minute as u8,
        second: f.second as u8,
        microsecond: f.ms * 1000,
        offset_seconds: Some(0),
        timezone_name: Some("UTC".to_owned()),
    })
}

/// How a date value prints in `console.log`: its ISO text, or `Invalid Date`.
pub(crate) fn inspect_value(t: f64) -> String {
    iso_string(t).unwrap_or_else(|| "Invalid Date".to_owned())
}
