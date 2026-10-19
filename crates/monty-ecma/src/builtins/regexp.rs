//! `RegExp` over `monty-regexp`: the constructor, `exec`/`test`, the flag
//! getters, the `Symbol.match`/`matchAll`/`replace`/`search`/`split` protocol
//! that `String.prototype` dispatches to, and the iterator `matchAll` returns.
//!
//! A `RegExp` object stores its pattern text and flags; the compiled program is
//! derived on first use (and again after a restore) and shared through an `Arc`.
//! Every match runs under the time budget through the executor's interrupt hook.

use std::{mem, ops::Range, sync::Arc};

use monty_regexp::{
    CompileError, CompileErrorSource, CompiledRegExp, ExecError, GroupName, RegExpFlags, RegExpMatch,
    javascript_compile_error_message,
};

use super::{
    NativeFn, arg, install_constructor, install_function, install_getter, install_symbol_function, install_tag, key,
    raw_object,
    string::{MatchContext, get_substitution},
    this_object,
};
use crate::{
    error::RunResult,
    heap::{HeapId, ObjectClass, PropFlags, PropertyKey, RegExpData},
    intern::WellKnownSymbol,
    string::JsString,
    value::JsValue,
    vm::{
        Vm,
        number::{to_uint32, to_usize},
    },
};

/// `ToLength`'s upper bound, 2⁵³ − 1.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    let prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.regexp_prototype = prototype;
    for native in [
        NativeFn::RegExpProtoExec,
        NativeFn::RegExpProtoTest,
        NativeFn::RegExpProtoToString,
    ] {
        install_function(vm, prototype, native);
    }
    for (name, native) in [
        ("source", NativeFn::RegExpProtoSource),
        ("flags", NativeFn::RegExpProtoFlags),
        ("global", NativeFn::RegExpProtoGlobal),
        ("ignoreCase", NativeFn::RegExpProtoIgnoreCase),
        ("multiline", NativeFn::RegExpProtoMultiline),
        ("dotAll", NativeFn::RegExpProtoDotAll),
        ("unicode", NativeFn::RegExpProtoUnicode),
        ("unicodeSets", NativeFn::RegExpProtoUnicodeSets),
        ("sticky", NativeFn::RegExpProtoSticky),
        ("hasIndices", NativeFn::RegExpProtoHasIndices),
    ] {
        install_getter(vm, prototype, name, native);
    }
    for (symbol, native) in [
        (WellKnownSymbol::Match, NativeFn::RegExpProtoMatch),
        (WellKnownSymbol::MatchAll, NativeFn::RegExpProtoMatchAll),
        (WellKnownSymbol::Replace, NativeFn::RegExpProtoReplace),
        (WellKnownSymbol::Search, NativeFn::RegExpProtoSearch),
        (WellKnownSymbol::Split, NativeFn::RegExpProtoSplit),
    ] {
        install_symbol_function(vm, prototype, symbol, native);
    }
    install_constructor(vm, global, NativeFn::RegExp, prototype);
    let iterator_prototype = vm.session.realm.iterator_prototype;
    let string_iterator_prototype = raw_object(vm, Some(iterator_prototype));
    vm.session.realm.regexp_string_iterator_prototype = string_iterator_prototype;
    install_function(vm, string_iterator_prototype, NativeFn::ArrayIteratorNext);
    install_tag(vm, string_iterator_prototype, "RegExp String Iterator");
}

/// `RegExp(pattern, flags)`, with or without `new`; `RegExp(re)` returns `re` itself.
pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    let pattern = arg(args, 0);
    let flags = arg(args, 1);
    let existing = pattern.heap_id().and_then(|id| match vm.object(id).map(|o| &o.class) {
        Some(ObjectClass::RegExp(data)) => Some((data.source.clone(), data.flags())),
        _ => None,
    });
    let (source, flags) = match existing {
        Some(_) if !construct && *flags == JsValue::Undefined => {
            return Ok(pattern.clone_with_heap(&vm.session.heap));
        }
        Some((source, existing_flags)) => {
            let flags = match flags {
                JsValue::Undefined => JsString::from(existing_flags.canonical_string().as_str()),
                other => vm.coerce_string(other)?,
            };
            (source, flags)
        }
        None => {
            let source = match pattern {
                JsValue::Undefined => JsString::empty(),
                other => vm.coerce_string(other)?,
            };
            let flags = match flags {
                JsValue::Undefined => JsString::empty(),
                other => vm.coerce_string(other)?,
            };
            (source, flags)
        }
    };
    create(vm, source, &flags)
}

/// A new `RegExp` object from pattern text and a flags string, or the
/// `SyntaxError` for an invalid pattern.
pub(crate) fn create(vm: &mut Vm<'_>, source: JsString, flags: &JsString) -> RunResult<JsValue> {
    let program = compile(vm, &source, flags)?;
    Ok(allocate(vm, source, program))
}

/// Allocates the object for an already compiled pattern, `lastIndex` at 0.
fn allocate(vm: &mut Vm<'_>, source: JsString, program: Arc<CompiledRegExp>) -> JsValue {
    let flags = program.flags().bits();
    let proto = vm.session.realm.regexp_prototype;
    let data = RegExpData {
        source,
        flags,
        program: Some(program),
    };
    let id = vm.new_class_object(proto, ObjectClass::RegExp(Box::new(data)));
    let last_index = key(vm, "lastIndex");
    vm.define_with_flags(id, last_index, JsValue::Number(0.0), PropFlags::WRITABLE);
    JsValue::Ref(id)
}

/// Compiles a pattern, charging the tracker for the program up front.
fn compile(vm: &mut Vm<'_>, source: &JsString, flags: &JsString) -> RunResult<Arc<CompiledRegExp>> {
    // the program is roughly linear in the pattern; the hard limit covers the rest
    vm.tracker().check_allocation(source.len().saturating_mul(64))?;
    match monty_regexp::compile(&source.to_units(), &flags.to_units()) {
        Ok(program) => Ok(Arc::new(program)),
        Err(error) => {
            let message = regexp_error_message(&source.to_string_lossy(), &flags.to_string_lossy(), &error);
            Err(vm.syntax_error(&message))
        }
    }
}

/// V8's shape for an invalid pattern or flags, carrying QuickJS's diagnostic text.
pub(crate) fn regexp_error_message(source: &str, flags: &str, error: &CompileError) -> String {
    if error.source() == CompileErrorSource::Flags {
        format!("Invalid flags supplied to RegExp constructor '{flags}'")
    } else {
        let message = javascript_compile_error_message(error);
        format!("Invalid regular expression: /{source}/{flags}: {message}")
    }
}

/// `/source/flags`, as `toString` and `console.log` print a regexp.
pub(crate) fn to_string(data: &RegExpData) -> String {
    format!(
        "/{}/{}",
        escape_source(&data.source).to_string_lossy(),
        data.flags().canonical_string()
    )
}

/// `EscapeRegExpPattern`: an empty pattern reads `(?:)`, and `/` outside a
/// character class plus line terminators are escaped so the text re-parses.
fn escape_source(source: &JsString) -> JsString {
    if source.is_empty() {
        return JsString::from("(?:)");
    }
    let mut out = Vec::with_capacity(source.len());
    let mut in_class = false;
    let mut escaped = false;
    for unit in source.units() {
        if escaped {
            escaped = false;
            out.push(unit);
        } else if unit == u16::from(b'\\') {
            escaped = true;
            out.push(unit);
        } else if unit == u16::from(b'[') {
            in_class = true;
            out.push(unit);
        } else if unit == u16::from(b']') {
            in_class = false;
            out.push(unit);
        } else if unit == u16::from(b'/') && !in_class {
            out.extend("\\/".encode_utf16());
        } else if unit == u16::from(b'\n') {
            out.extend("\\n".encode_utf16());
        } else if unit == u16::from(b'\r') {
            out.extend("\\r".encode_utf16());
        } else if unit == 0x2028 {
            out.extend("\\u2028".encode_utf16());
        } else if unit == 0x2029 {
            out.extend("\\u2029".encode_utf16());
        } else {
            out.push(unit);
        }
    }
    JsString::from_units(&out)
}

// === Receiver access ===

/// The regexp a method was called on, or V8's incompatible-receiver error.
fn receiver(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<HeapId> {
    if let JsValue::Ref(id) = this
        && matches!(vm.object(*id).map(|o| &o.class), Some(ObjectClass::RegExp(_)))
    {
        return Ok(*id);
    }
    let shown = vm.inspect_short(this)?;
    Err(vm.type_error(&format!(
        "Method RegExp.prototype.{method} called on incompatible receiver {shown}"
    )))
}

fn data<'a>(vm: &'a Vm<'_>, id: HeapId) -> &'a RegExpData {
    match vm.object(id).map(|o| &o.class) {
        Some(ObjectClass::RegExp(data)) => data,
        _ => unreachable!("checked by `receiver`"),
    }
}

/// The compiled program of a regexp, rebuilt from its source after a restore.
fn program(vm: &mut Vm<'_>, id: HeapId) -> RunResult<Arc<CompiledRegExp>> {
    let data = data(vm, id);
    if let Some(program) = &data.program {
        return Ok(Arc::clone(program));
    }
    let (source, flags) = (data.source.clone(), data.flags().canonical_string());
    let program = compile(vm, &source, &JsString::from(flags.as_str()))?;
    if let Some(ObjectClass::RegExp(data)) = vm.object_mut(id).map(|o| &mut o.class) {
        data.program = Some(Arc::clone(&program));
    }
    Ok(program)
}

/// `ToLength(Get(R, "lastIndex"))`.
fn last_index(vm: &mut Vm<'_>, id: HeapId) -> RunResult<usize> {
    let key = key(vm, "lastIndex");
    let value = vm.get(&JsValue::Ref(id), key)?;
    let number = vm.coerce_integer(&value);
    value.drop_with(&mut vm.session.heap);
    Ok(to_usize(number?.clamp(0.0, MAX_SAFE_INTEGER)))
}

/// `Set(R, "lastIndex", index, true)`: throws when the property is read-only.
fn set_last_index(vm: &mut Vm<'_>, id: HeapId, index: usize) -> RunResult<()> {
    let key = key(vm, "lastIndex");
    vm.set(&JsValue::Ref(id), key, JsValue::Number(index as f64))
}

// === Matching ===

/// Runs `program` on `input` from `start`, polling the time budget.
fn execute(
    vm: &mut Vm<'_>,
    program: &CompiledRegExp,
    input: &JsString,
    start: usize,
) -> RunResult<Option<RegExpMatch>> {
    let outcome = {
        let tracker = vm.tracker();
        let interrupted = || tracker.check_time().is_err();
        match input {
            JsString::Latin1(bytes) => monty_regexp::execute_latin1_with_interrupt(program, bytes, start, interrupted),
            JsString::Utf16(units) => monty_regexp::execute_with_interrupt(program, units, start, interrupted),
        }
    };
    match outcome {
        Ok(found) => Ok(found),
        Err(ExecError::Interrupted) => {
            vm.tracker().check_time()?;
            Err(vm.internal_error("regular expression interrupted"))
        }
        Err(ExecError::OutOfMemory) => Err(vm.range_error("Maximum call stack size exceeded")),
        Err(error) => Err(vm.internal_error(&error.to_string())),
    }
}

/// `RegExpBuiltinExec` up to the match: reads and updates `lastIndex` when
/// the pattern is global or sticky.
fn exec_raw(vm: &mut Vm<'_>, id: HeapId, input: &JsString) -> RunResult<Option<RegExpMatch>> {
    let program = program(vm, id)?;
    let flags = program.flags();
    let tracks_position = flags.contains(RegExpFlags::GLOBAL) || flags.contains(RegExpFlags::STICKY);
    let start = if tracks_position { last_index(vm, id)? } else { 0 };
    if start > input.len() {
        if tracks_position {
            set_last_index(vm, id, 0)?;
        }
        return Ok(None);
    }
    let found = execute(vm, &program, input, start)?;
    if tracks_position {
        let end = found.as_ref().and_then(|m| m.capture(0)).map_or(0, |r| r.end);
        set_last_index(vm, id, end)?;
    }
    Ok(found)
}

/// `RegExpBuiltinExec`: the match array, or `null`.
fn exec_value(vm: &mut Vm<'_>, id: HeapId, input: &JsString) -> RunResult<JsValue> {
    match exec_raw(vm, id, input)? {
        None => Ok(JsValue::Null),
        Some(found) => match_array(vm, id, &found, input),
    }
}

/// The match array: the captures as elements, then `index`, `input`,
/// `groups` and (with the `d` flag) `indices`.
fn match_array(vm: &mut Vm<'_>, id: HeapId, found: &RegExpMatch, input: &JsString) -> RunResult<JsValue> {
    let program = program(vm, id)?;
    let captures = found.captures();
    let mut items = Vec::with_capacity(captures.len());
    for range in captures {
        items.push(capture_value(vm, input, range.as_ref())?);
    }
    let array = vm.new_array(items)?;
    let array_id = array.heap_id().expect("array");
    let start = found.capture(0).map_or(0, |r| r.start);
    define(vm, array_id, "index", JsValue::Number(start as f64));
    let input_value = vm.alloc_string(input.clone())?;
    define(vm, array_id, "input", input_value);
    let groups = match program.group_names() {
        None => JsValue::Undefined,
        Some(names) => groups_object(vm, names, |vm, index| capture_value(vm, input, found.capture(index)))?,
    };
    define(vm, array_id, "groups", groups);
    if program.flags().contains(RegExpFlags::HAS_INDICES) {
        let mut pairs = Vec::with_capacity(captures.len());
        for range in captures {
            pairs.push(index_pair(vm, range.as_ref())?);
        }
        let indices = vm.new_array(pairs)?;
        let indices_id = indices.heap_id().expect("array");
        let groups = match program.group_names() {
            None => JsValue::Undefined,
            Some(names) => groups_object(vm, names, |vm, index| index_pair(vm, found.capture(index)))?,
        };
        define(vm, indices_id, "groups", groups);
        define(vm, array_id, "indices", indices);
    }
    Ok(array)
}

/// The text of one capture, `undefined` when the group did not participate.
fn capture_value(vm: &mut Vm<'_>, input: &JsString, range: Option<&Range<usize>>) -> RunResult<JsValue> {
    match range {
        Some(range) => vm.alloc_string(input.slice(range.start, range.end)),
        None => Ok(JsValue::Undefined),
    }
}

/// A `[start, end]` pair for the `indices` array.
fn index_pair(vm: &mut Vm<'_>, range: Option<&Range<usize>>) -> RunResult<JsValue> {
    match range {
        Some(range) => vm.new_array(vec![
            JsValue::Number(range.start as f64),
            JsValue::Number(range.end as f64),
        ]),
        None => Ok(JsValue::Undefined),
    }
}

/// The `groups` object: null prototype, one property per named group, whose
/// value `value_of` produces from the capture's index.
fn groups_object(
    vm: &mut Vm<'_>,
    names: &[Option<GroupName>],
    mut value_of: impl FnMut(&mut Vm<'_>, usize) -> RunResult<JsValue>,
) -> RunResult<JsValue> {
    let object = vm.new_object_with_proto(None);
    let object_id = object.heap_id().expect("object");
    for (index, name) in names.iter().enumerate() {
        let Some(name) = name else {
            continue;
        };
        let value = match value_of(vm, index + 1) {
            Ok(value) => value,
            Err(err) => {
                object.drop_with(&mut vm.session.heap);
                return Err(err);
            }
        };
        let atom = vm.session.interns.intern_untracked(JsString::from_units(name));
        vm.define_data(object_id, PropertyKey::Atom(atom), value);
    }
    Ok(object)
}

fn define(vm: &mut Vm<'_>, id: HeapId, name: &str, value: JsValue) {
    let key = key(vm, name);
    vm.define_data(id, key, value);
}

/// `AdvanceStringIndex`: one code point in unicode mode, one code unit otherwise.
fn advance_index(input: &JsString, index: usize, unicode: bool) -> usize {
    let is_pair = unicode
        && matches!(
            (input.unit_at(index), input.unit_at(index + 1)),
            (Some(high), Some(low)) if (0xD800..0xDC00).contains(&high) && (0xDC00..0xE000).contains(&low)
        );
    if is_pair { index + 2 } else { index + 1 }
}

/// Whether a match consumed nothing, so the next search must move past it.
fn is_empty_match(found: &RegExpMatch) -> bool {
    found.capture(0).is_none_or(Range::is_empty)
}

// === RegExp.prototype ===

pub(super) fn exec(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, "exec")?;
    let input = vm.coerce_string(arg(args, 0))?;
    exec_value(vm, id, &input)
}

pub(super) fn test(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, "test")?;
    let input = vm.coerce_string(arg(args, 0))?;
    Ok(JsValue::Bool(exec_raw(vm, id, &input)?.is_some()))
}

/// `toString`: `/source/flags` through the `source` and `flags` getters, so
/// it also works on `RegExp.prototype` itself.
pub(super) fn proto_to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    this_object(vm, this, "RegExp.prototype.toString")?;
    let source = key(vm, "source");
    let source = vm.get(this, source)?;
    let source_text = vm.coerce_string(&source);
    source.drop_with(&mut vm.session.heap);
    let flags = key(vm, "flags");
    let flags = vm.get(this, flags)?;
    let flags_text = vm.coerce_string(&flags);
    flags.drop_with(&mut vm.session.heap);
    let text = format!("/{}/{}", source_text?.to_string_lossy(), flags_text?.to_string_lossy());
    vm.alloc_string(JsString::from(text.as_str()))
}

/// The regexp a getter was called on; `None` for `RegExp.prototype` itself,
/// which reports `undefined` flags rather than throwing.
fn getter_receiver(vm: &mut Vm<'_>, this: &JsValue, name: &str) -> RunResult<Option<HeapId>> {
    match this {
        JsValue::Ref(id) if matches!(vm.object(*id).map(|o| &o.class), Some(ObjectClass::RegExp(_))) => Ok(Some(*id)),
        JsValue::Ref(id) if *id == vm.session.realm.regexp_prototype => Ok(None),
        _ => Err(vm.type_error(&format!("RegExp.prototype.{name} getter called on non-RegExp object"))),
    }
}

pub(super) fn source(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let source = match getter_receiver(vm, this, "source")? {
        Some(id) => escape_source(&data(vm, id).source),
        None => JsString::from("(?:)"),
    };
    vm.alloc_string(source)
}

pub(super) fn flags(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let flags = match getter_receiver(vm, this, "flags")? {
        Some(id) => data(vm, id).flags().canonical_string(),
        None => String::new(),
    };
    Ok(vm.atom_value(&flags))
}

/// One boolean flag getter.
fn flag_getter(vm: &mut Vm<'_>, this: &JsValue, name: &str, flag: RegExpFlags) -> RunResult<JsValue> {
    Ok(match getter_receiver(vm, this, name)? {
        Some(id) => JsValue::Bool(data(vm, id).flags().contains(flag)),
        None => JsValue::Undefined,
    })
}

pub(super) fn global(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "global", RegExpFlags::GLOBAL)
}

pub(super) fn ignore_case(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "ignoreCase", RegExpFlags::IGNORE_CASE)
}

pub(super) fn multiline(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "multiline", RegExpFlags::MULTILINE)
}

pub(super) fn dot_all(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "dotAll", RegExpFlags::DOT_ALL)
}

pub(super) fn unicode(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "unicode", RegExpFlags::UNICODE)
}

pub(super) fn unicode_sets(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "unicodeSets", RegExpFlags::UNICODE_SETS)
}

pub(super) fn sticky(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "sticky", RegExpFlags::STICKY)
}

pub(super) fn has_indices(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    flag_getter(vm, this, "hasIndices", RegExpFlags::HAS_INDICES)
}

// === The Symbol protocol ===

/// `[Symbol.match]`: the match array for a non-global pattern, else every
/// matched text (or `null`).
pub(super) fn symbol_match(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, "exec")?;
    let input = vm.coerce_string(arg(args, 0))?;
    let flags = data(vm, id).flags();
    if !flags.contains(RegExpFlags::GLOBAL) {
        return exec_value(vm, id, &input);
    }
    set_last_index(vm, id, 0)?;
    let mut matches = Vec::new();
    if let Err(err) = collect_matches(vm, id, &input, flags.is_unicode(), &mut matches) {
        vm.drop_all(JsValue::Undefined, matches);
        return Err(err);
    }
    if matches.is_empty() {
        Ok(JsValue::Null)
    } else {
        vm.new_array(matches)
    }
}

/// Every match's text of a global pattern from its current `lastIndex`.
fn collect_matches(
    vm: &mut Vm<'_>,
    id: HeapId,
    input: &JsString,
    unicode: bool,
    matches: &mut Vec<JsValue>,
) -> RunResult<()> {
    while let Some(found) = exec_raw(vm, id, input)? {
        let range = found.capture(0).cloned().unwrap_or(0..0);
        matches.push(vm.alloc_string(input.slice(range.start, range.end))?);
        if range.is_empty() {
            let next = advance_index(input, last_index(vm, id)?, unicode);
            set_last_index(vm, id, next)?;
        }
        vm.tracker().check_memory_time_every(matches.len())?;
    }
    Ok(())
}

/// `[Symbol.search]`: the index of the first match, leaving `lastIndex` as it was.
pub(super) fn symbol_search(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, "exec")?;
    let input = vm.coerce_string(arg(args, 0))?;
    let key = key(vm, "lastIndex");
    let previous = vm.get(this, key)?;
    if !vm.same_value(&previous, &JsValue::Number(0.0)) {
        set_last_index(vm, id, 0)?;
    }
    let found = exec_raw(vm, id, &input);
    let current = vm.get(this, key)?;
    let restore = if vm.same_value(&current, &previous) {
        previous.drop_with(&mut vm.session.heap);
        Ok(())
    } else {
        vm.set(this, key, previous)
    };
    current.drop_with(&mut vm.session.heap);
    let found = found?;
    restore?;
    Ok(JsValue::Number(
        found.and_then(|m| m.capture(0).map(|r| r.start as f64)).unwrap_or(-1.0),
    ))
}

/// `[Symbol.replace]`: every match (or the first, without `g`) replaced by a
/// template expansion or a callback's result.
pub(super) fn symbol_replace(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, "exec")?;
    let input = vm.coerce_string(arg(args, 0))?;
    let replace_value = arg(args, 1);
    let template = if vm.is_callable(replace_value) {
        None
    } else {
        Some(vm.coerce_string(replace_value)?.to_units())
    };
    let flags = data(vm, id).flags();
    let global = flags.contains(RegExpFlags::GLOBAL);
    if global {
        set_last_index(vm, id, 0)?;
    }
    let mut results = Vec::new();
    while let Some(found) = exec_raw(vm, id, &input)? {
        let empty = is_empty_match(&found);
        results.push(found);
        if !global {
            break;
        }
        if empty {
            let next = advance_index(&input, last_index(vm, id)?, flags.is_unicode());
            set_last_index(vm, id, next)?;
        }
        vm.tracker().check_memory_time_every(results.len())?;
    }
    let program = program(vm, id)?;
    let units = input.to_units();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut next_source_position = 0;
    for found in &results {
        let whole = found.capture(0).cloned().unwrap_or(0..0);
        let captures: Vec<Option<JsString>> = (1..found.captures().len())
            .map(|i| found.capture(i).map(|r| input.slice(r.start, r.end)))
            .collect();
        let groups = match program.group_names() {
            None => None,
            Some(names) => Some(groups_object(vm, names, |vm, index| {
                capture_value(vm, &input, found.capture(index))
            })?),
        };
        let replacement = match &template {
            Some(template) => {
                let context = MatchContext {
                    subject: &units,
                    position: whole.start,
                    matched: &units[whole.clone()],
                    captures: &captures,
                    groups: groups.as_ref(),
                };
                let mut expanded = Vec::new();
                get_substitution(vm, template, &context, &mut expanded).map(|()| expanded)
            }
            None => call_replacer(vm, replace_value, &input, &whole, &captures, groups.as_ref()),
        };
        if let Some(groups) = groups {
            groups.drop_with(&mut vm.session.heap);
        }
        let replacement = replacement?;
        if whole.start >= next_source_position {
            out.extend_from_slice(&units[next_source_position..whole.start]);
            out.extend(replacement);
            next_source_position = whole.end;
        }
        vm.tracker().check_growth(out.len(), out.capacity(), 2)?;
    }
    if next_source_position < units.len() {
        out.extend_from_slice(&units[next_source_position..]);
    }
    vm.alloc_string(JsString::from_units(&out))
}

/// Calls a replacement function with `(match, …captures, position, input[, groups])`.
fn call_replacer(
    vm: &mut Vm<'_>,
    replacer: &JsValue,
    input: &JsString,
    whole: &Range<usize>,
    captures: &[Option<JsString>],
    groups: Option<&JsValue>,
) -> RunResult<Vec<u16>> {
    let mut call_args = Vec::with_capacity(captures.len() + 4);
    call_args.push(vm.alloc_string(input.slice(whole.start, whole.end))?);
    for capture in captures {
        call_args.push(match capture {
            Some(text) => vm.alloc_string(text.clone())?,
            None => JsValue::Undefined,
        });
    }
    call_args.push(JsValue::Number(whole.start as f64));
    call_args.push(vm.alloc_string(input.clone())?);
    if let Some(groups) = groups {
        call_args.push(groups.clone_with_heap(&vm.session.heap));
    }
    let result = vm.call(
        replacer.clone_with_heap(&vm.session.heap),
        JsValue::Undefined,
        call_args,
    )?;
    let text = vm.coerce_string(&result);
    result.drop_with(&mut vm.session.heap);
    Ok(text?.to_units())
}

/// `[Symbol.split]`: splits on a sticky copy of the pattern so a separator is
/// only taken exactly at the cursor; captures join the result.
pub(super) fn symbol_split(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, "exec")?;
    let input = vm.coerce_string(arg(args, 0))?;
    let limit = match arg(args, 1) {
        JsValue::Undefined => u32::MAX as usize,
        other => to_uint32(vm.coerce_number(other)?) as usize,
    };
    let (source, flags) = {
        let data = data(vm, id);
        (data.source.clone(), data.flags())
    };
    let unicode = flags.is_unicode();
    let mut flag_text = flags.canonical_string();
    if !flags.contains(RegExpFlags::STICKY) {
        flag_text.push('y');
    }
    let splitter = compile(vm, &source, &JsString::from(flag_text.as_str()))?;
    let mut parts = Vec::new();
    if limit == 0 {
        return vm.new_array(parts);
    }
    let len = input.len();
    if len == 0 {
        if execute(vm, &splitter, &input, 0)?.is_none() {
            parts.push(vm.alloc_string(input)?);
        }
        return vm.new_array(parts);
    }
    let mut p = 0;
    let mut q = 0;
    while q < len {
        match execute(vm, &splitter, &input, q)? {
            None => q = advance_index(&input, q, unicode),
            Some(found) => {
                let end = found.capture(0).map_or(q, |r| r.end).min(len);
                if end == p {
                    q = advance_index(&input, q, unicode);
                } else {
                    parts.push(vm.alloc_string(input.slice(p, q))?);
                    if parts.len() == limit {
                        return vm.new_array(parts);
                    }
                    p = end;
                    for index in 1..found.captures().len() {
                        parts.push(capture_value(vm, &input, found.capture(index))?);
                        if parts.len() == limit {
                            return vm.new_array(parts);
                        }
                    }
                    q = p;
                }
            }
        }
        vm.tracker().check_memory_time_every(q)?;
    }
    parts.push(vm.alloc_string(input.slice(p, len))?);
    vm.new_array(parts)
}

/// `[Symbol.matchAll]`: an iterator over a private copy of the pattern that
/// starts from this regexp's `lastIndex`.
pub(super) fn symbol_match_all(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, "exec")?;
    let input = vm.coerce_string(arg(args, 0))?;
    let program = program(vm, id)?;
    let source = data(vm, id).source.clone();
    let matcher = allocate(vm, source, program);
    let matcher_id = matcher.heap_id().expect("object");
    let start = last_index(vm, id)?;
    set_last_index(vm, matcher_id, start)?;
    let proto = vm.session.realm.regexp_string_iterator_prototype;
    let iterator = vm.new_class_object(
        proto,
        ObjectClass::RegExpStringIterator {
            regexp: matcher_id,
            string: input,
            done: false,
        },
    );
    Ok(JsValue::Ref(iterator))
}

/// Advances a `RegExp String Iterator`: the next match array, or `None` once
/// exhausted. The input is taken out of the iterator while matching so it is
/// not copied per step, and put back afterwards.
pub(crate) fn iterator_step(vm: &mut Vm<'_>, id: HeapId) -> RunResult<Option<JsValue>> {
    let (regexp, input, done) = match vm.object_mut(id).map(|o| &mut o.class) {
        Some(ObjectClass::RegExpStringIterator { regexp, string, done }) => {
            (*regexp, mem::replace(string, JsString::empty()), *done)
        }
        _ => unreachable!("checked by the caller"),
    };
    if done {
        restore_iterator(vm, id, input, true);
        return Ok(None);
    }
    let step = iterator_match(vm, regexp, &input);
    let finished = !matches!(step, Ok(Some(_))) || !data(vm, regexp).flags().contains(RegExpFlags::GLOBAL);
    restore_iterator(vm, id, input, finished);
    step
}

/// One `matchAll` step on the iterator's private regexp.
fn iterator_match(vm: &mut Vm<'_>, regexp: HeapId, input: &JsString) -> RunResult<Option<JsValue>> {
    let flags = data(vm, regexp).flags();
    let Some(found) = exec_raw(vm, regexp, input)? else {
        return Ok(None);
    };
    if flags.contains(RegExpFlags::GLOBAL) && is_empty_match(&found) {
        let next = advance_index(input, last_index(vm, regexp)?, flags.is_unicode());
        set_last_index(vm, regexp, next)?;
    }
    match_array(vm, regexp, &found, input).map(Some)
}

fn restore_iterator(vm: &mut Vm<'_>, id: HeapId, input: JsString, finished: bool) {
    if let Some(ObjectClass::RegExpStringIterator { string, done, .. }) = vm.object_mut(id).map(|o| &mut o.class) {
        *string = input;
        *done = finished;
    }
}
