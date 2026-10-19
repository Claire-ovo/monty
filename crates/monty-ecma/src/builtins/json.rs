//! `JSON.stringify` and `JSON.parse`.

use std::{fmt::Write, slice};

use super::{NativeFn, arg, install_function, install_value, plain_object};
use crate::{
    error::{RunResult, Throw},
    heap::{HeapId, ObjectClass, PropFlags, PropertyKey, Slot},
    intern::StaticAtom,
    string::JsString,
    value::JsValue,
    vm::{
        Vm,
        number::{index_u32, number_to_string, to_usize},
    },
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let json = plain_object(vm);
    install_function(vm, json, NativeFn::JsonStringify);
    install_function(vm, json, NativeFn::JsonParse);
    install_value(vm, global, "JSON", JsValue::Ref(json), PropFlags::HIDDEN);
}

pub(super) fn stringify(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let indent = match arg(args, 2) {
        JsValue::Number(n) => " ".repeat(to_usize(n.clamp(0.0, 10.0))),
        value @ (JsValue::Atom(_) | JsValue::Str(_)) => {
            let text = vm.string_of(value).expect("string").to_string_lossy();
            text.chars().take(10).collect()
        }
        _ => String::new(),
    };
    let replacer = arg(args, 1);
    let replacer = if vm.is_callable(replacer) { Some(replacer) } else { None };
    let mut serializer = Serializer {
        indent,
        replacer,
        stack: Vec::new(),
        out: String::new(),
    };
    let holder = vm.new_object();
    let JsValue::Ref(holder_id) = &holder else {
        unreachable!()
    };
    let key = vm.atom_value("");
    let value = arg(args, 0).clone_with_heap(&vm.session.heap);
    vm.define_data(*holder_id, PropertyKey::Atom(StaticAtom::Empty.id()), value);
    let written = serializer.value(vm, &holder, key, 0);
    holder.drop_with(&mut vm.session.heap);
    if written? {
        let text = serializer.out;
        vm.alloc_string(JsString::from(text.as_str()))
    } else {
        Ok(JsValue::Undefined)
    }
}

/// `JSON.stringify(value)` as Rust text, `None` for `undefined`.
pub(crate) fn stringify_to_string(vm: &mut Vm<'_>, value: &JsValue) -> RunResult<Option<String>> {
    let result = stringify(vm, &JsValue::Undefined, slice::from_ref(value), false)?;
    let text = vm.string_of(&result).map(|s| s.to_string_lossy());
    result.drop_with(&mut vm.session.heap);
    Ok(text)
}

/// The serializer state: output buffer, indentation, and the objects on the
/// current path (for the circular-structure error).
struct Serializer<'a> {
    indent: String,
    replacer: Option<&'a JsValue>,
    stack: Vec<HeapId>,
    out: String,
}

impl Serializer<'_> {
    /// `SerializeJSONProperty`: writes `holder[key]`, returning whether anything was written.
    fn value(&mut self, vm: &mut Vm<'_>, holder: &JsValue, key: JsValue, depth: usize) -> RunResult<bool> {
        let property_key = vm.coerce_key(key.clone_with_heap(&vm.session.heap))?;
        let mut value = vm.get(holder, property_key)?;
        if value.is_object() {
            let to_json_name = vm.session.interns.intern_str("toJSON");
            let to_json = vm.get_atom(&value, to_json_name)?;
            if vm.is_callable(&to_json) {
                let key_value = key.clone_with_heap(&vm.session.heap);
                value = vm.call(to_json, value, vec![key_value])?;
            } else {
                to_json.drop_with(&mut vm.session.heap);
            }
        }
        if let Some(replacer) = self.replacer {
            let key_value = key.clone_with_heap(&vm.session.heap);
            value = vm.call(
                replacer.clone_with_heap(&vm.session.heap),
                holder.clone_with_heap(&vm.session.heap),
                vec![key_value, value],
            )?;
        }
        key.drop_with(&mut vm.session.heap);
        // unwrap boxed primitives
        if let JsValue::Ref(id) = &value {
            let unboxed = match vm.object(*id).map(|o| &o.class) {
                Some(ObjectClass::BoxedNumber(n)) => Some(JsValue::Number(*n)),
                Some(ObjectClass::BoxedBool(b)) => Some(JsValue::Bool(*b)),
                Some(ObjectClass::BoxedString(s)) => {
                    let s = s.clone();
                    Some(vm.alloc_string(s)?)
                }
                _ => None,
            };
            if let Some(unboxed) = unboxed {
                value.drop_with(&mut vm.session.heap);
                value = unboxed;
            }
        }
        let written = match &value {
            JsValue::Null => {
                self.out.push_str("null");
                true
            }
            JsValue::Bool(b) => {
                self.out.push_str(if *b { "true" } else { "false" });
                true
            }
            JsValue::Number(n) => {
                if n.is_finite() {
                    self.out.push_str(&number_to_string(*n));
                } else {
                    self.out.push_str("null");
                }
                true
            }
            JsValue::Atom(_) | JsValue::Str(_) => {
                let text = vm.string_of(&value).expect("string").to_string_lossy();
                quote_json(&text, &mut self.out);
                true
            }
            JsValue::Ref(id) if !vm.is_callable_id(*id) => {
                let id = *id;
                if self.stack.contains(&id) {
                    value.drop_with(&mut vm.session.heap);
                    return Err(vm.type_error("Converting circular structure to JSON"));
                }
                self.stack.push(id);
                let result = if vm.is_array(&value) {
                    self.array(vm, &value, id, depth)
                } else {
                    self.object(vm, &value, id, depth)
                };
                self.stack.pop();
                result?;
                true
            }
            _ => false,
        };
        value.drop_with(&mut vm.session.heap);
        vm.tracker().check_growth(self.out.len(), self.out.capacity(), 1)?;
        Ok(written)
    }

    fn newline(&mut self, depth: usize) {
        if !self.indent.is_empty() {
            self.out.push('\n');
            for _ in 0..depth {
                self.out.push_str(&self.indent);
            }
        }
    }

    fn array(&mut self, vm: &mut Vm<'_>, array: &JsValue, id: HeapId, depth: usize) -> RunResult<()> {
        let length = vm.object(id).map_or(0, |o| o.elements.len());
        if length == 0 {
            self.out.push_str("[]");
            return Ok(());
        }
        self.out.push('[');
        for index in 0..length {
            if index > 0 {
                self.out.push(',');
            }
            self.newline(depth + 1);
            let key = vm.atom_value(&index.to_string());
            if !self.value(vm, array, key, depth + 1)? {
                self.out.push_str("null");
            }
        }
        self.newline(depth);
        self.out.push(']');
        Ok(())
    }

    fn object(&mut self, vm: &mut Vm<'_>, object: &JsValue, id: HeapId, depth: usize) -> RunResult<()> {
        let keys = vm.own_keys(id, true);
        self.out.push('{');
        let mut first = true;
        for key in keys {
            if let Some(Slot::Accessor { getter, .. }) = vm.object(id).and_then(|o| o.props.get(&key))
                && getter.is_nullish()
            {
                continue;
            }
            let mark = self.out.len();
            if !first {
                self.out.push(',');
            }
            self.newline(depth + 1);
            let name = vm.key_display(key);
            quote_json(&name, &mut self.out);
            self.out.push(':');
            if !self.indent.is_empty() {
                self.out.push(' ');
            }
            let key_value = vm.key_to_value(key);
            if self.value(vm, object, key_value, depth + 1)? {
                first = false;
            } else {
                self.out.truncate(mark);
            }
        }
        if first {
            self.out.push('}');
        } else {
            self.newline(depth);
            self.out.push('}');
        }
        Ok(())
    }
}

/// Writes a JSON string literal.
fn quote_json(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub(super) fn parse(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let text = vm.coerce_string(arg(args, 0))?.to_string_lossy();
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
        text: &text,
    };
    parser.skip_whitespace();
    let value = parser.value(vm, 0)?;
    parser.skip_whitespace();
    if parser.pos < parser.bytes.len() {
        value.drop_with(&mut vm.session.heap);
        return Err(parser.error(vm, "Unexpected non-whitespace character after JSON"));
    }
    let reviver = arg(args, 1);
    if vm.is_callable(reviver) {
        let holder = vm.new_object();
        let JsValue::Ref(holder_id) = &holder else {
            unreachable!()
        };
        vm.define_data(*holder_id, PropertyKey::Atom(StaticAtom::Empty.id()), value);
        let key = JsValue::Atom(StaticAtom::Empty.id());
        let result = revive(vm, reviver, &holder, key);
        holder.drop_with(&mut vm.session.heap);
        return result;
    }
    Ok(value)
}

/// `InternalizeJSONProperty`: applies the reviver bottom-up.
fn revive(vm: &mut Vm<'_>, reviver: &JsValue, holder: &JsValue, key: JsValue) -> RunResult<JsValue> {
    let property_key = vm.coerce_key(key.clone_with_heap(&vm.session.heap))?;
    let value = vm.get(holder, property_key)?;
    if let JsValue::Ref(id) = &value {
        let id = *id;
        let keys: Vec<PropertyKey> = if vm.is_array(&value) {
            (0..vm.object(id).map_or(0, |o| o.elements.len()))
                .map(|i| PropertyKey::Index(index_u32(i)))
                .collect()
        } else {
            vm.own_keys(id, true)
        };
        for child_key in keys {
            let child_name = vm.key_to_value(child_key);
            let revived = revive(vm, reviver, &value, child_name)?;
            if revived == JsValue::Undefined {
                vm.delete(&value, child_key)?;
            } else {
                vm.define_data(id, child_key, revived);
            }
        }
    }
    vm.call(
        reviver.clone_with_heap(&vm.session.heap),
        holder.clone_with_heap(&vm.session.heap),
        vec![key, value],
    )
}

/// A recursive-descent JSON reader producing sandbox values.
struct Parser<'t> {
    bytes: &'t [u8],
    pos: usize,
    text: &'t str,
}

/// Nesting beyond this is a `SyntaxError`, protecting the Rust stack.
const MAX_JSON_DEPTH: usize = 512;

impl Parser<'_> {
    fn skip_whitespace(&mut self) {
        while self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn error(&self, vm: &mut Vm<'_>, message: &str) -> Throw {
        let _ = self.text;
        vm.syntax_error(&format!("{message} in JSON at position {}", self.pos))
    }

    fn unexpected(&self, vm: &mut Vm<'_>) -> Throw {
        match self.bytes.get(self.pos) {
            None => vm.syntax_error("Unexpected end of JSON input"),
            Some(byte) => {
                let shown = self.text[self.pos..].chars().next().unwrap_or(char::from(*byte));
                vm.syntax_error(&format!(
                    "Unexpected token '{shown}', \"{}\" is not valid JSON",
                    preview(self.text)
                ))
            }
        }
    }

    fn value(&mut self, vm: &mut Vm<'_>, depth: usize) -> RunResult<JsValue> {
        if depth > MAX_JSON_DEPTH {
            return Err(vm.range_error("Maximum call stack size exceeded"));
        }
        vm.tracker().check_memory_time_every(self.pos)?;
        match self.bytes.get(self.pos) {
            Some(b'{') => self.object(vm, depth),
            Some(b'[') => self.array(vm, depth),
            Some(b'"') => {
                let string = self.string(vm)?;
                vm.alloc_string(string)
            }
            Some(b't') => self.literal(vm, "true", JsValue::Bool(true)),
            Some(b'f') => self.literal(vm, "false", JsValue::Bool(false)),
            Some(b'n') => self.literal(vm, "null", JsValue::Null),
            Some(b'-' | b'0'..=b'9') => self.number(vm),
            _ => Err(self.unexpected(vm)),
        }
    }

    fn literal(&mut self, vm: &mut Vm<'_>, word: &str, value: JsValue) -> RunResult<JsValue> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.unexpected(vm))
        }
    }

    fn number(&mut self, vm: &mut Vm<'_>) -> RunResult<JsValue> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos == digits_start || (self.bytes[digits_start] == b'0' && self.pos - digits_start > 1) {
            return Err(self.unexpected(vm));
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            let fraction_start = self.pos;
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            if self.pos == fraction_start {
                return Err(self.unexpected(vm));
            }
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exponent_start = self.pos;
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            if self.pos == exponent_start {
                return Err(self.unexpected(vm));
            }
        }
        let number: f64 = self.text[start..self.pos].parse().map_err(|_| self.unexpected(vm))?;
        Ok(JsValue::Number(number))
    }

    fn string(&mut self, vm: &mut Vm<'_>) -> RunResult<JsString> {
        self.pos += 1; // opening quote
        let mut units: Vec<u16> = Vec::new();
        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err(vm.syntax_error(&format!("Unterminated string in JSON at position {}", self.pos)));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(JsString::from_units(&units));
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(&escape) = self.bytes.get(self.pos) else {
                        return Err(self.unexpected(vm));
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => units.push(u16::from(b'"')),
                        b'\\' => units.push(u16::from(b'\\')),
                        b'/' => units.push(u16::from(b'/')),
                        b'b' => units.push(8),
                        b'f' => units.push(12),
                        b'n' => units.push(10),
                        b'r' => units.push(13),
                        b't' => units.push(9),
                        b'u' => {
                            let hex = self
                                .text
                                .get(self.pos..self.pos + 4)
                                .ok_or_else(|| self.unexpected(vm))?;
                            let unit = u16::from_str_radix(hex, 16).map_err(|_| self.unexpected(vm))?;
                            self.pos += 4;
                            units.push(unit);
                        }
                        _ => {
                            self.pos -= 1;
                            return Err(self.unexpected(vm));
                        }
                    }
                }
                0..=0x1F => return Err(self.unexpected(vm)),
                _ => {
                    let c = self.text[self.pos..].chars().next().expect("valid utf-8");
                    let mut buffer = [0u16; 2];
                    units.extend_from_slice(c.encode_utf16(&mut buffer));
                    self.pos += c.len_utf8();
                }
            }
            vm.tracker().check_growth(units.len(), units.capacity(), 2)?;
        }
    }

    fn array(&mut self, vm: &mut Vm<'_>, depth: usize) -> RunResult<JsValue> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return vm.new_array(items);
        }
        loop {
            self.skip_whitespace();
            match self.value(vm, depth + 1) {
                Ok(value) => items.push(value),
                Err(err) => {
                    for item in items {
                        item.drop_with(&mut vm.session.heap);
                    }
                    return Err(err);
                }
            }
            self.skip_whitespace();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return vm.new_array(items);
                }
                _ => {
                    for item in items {
                        item.drop_with(&mut vm.session.heap);
                    }
                    return Err(self.unexpected(vm));
                }
            }
        }
    }

    fn object(&mut self, vm: &mut Vm<'_>, depth: usize) -> RunResult<JsValue> {
        self.pos += 1;
        let object = vm.new_object();
        let JsValue::Ref(id) = &object else { unreachable!() };
        let id = *id;
        self.skip_whitespace();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(object);
        }
        loop {
            self.skip_whitespace();
            if self.bytes.get(self.pos) != Some(&b'"') {
                object.drop_with(&mut vm.session.heap);
                return Err(self.unexpected(vm));
            }
            let key = match self.string(vm) {
                Ok(key) => key,
                Err(err) => {
                    object.drop_with(&mut vm.session.heap);
                    return Err(err);
                }
            };
            self.skip_whitespace();
            if self.bytes.get(self.pos) != Some(&b':') {
                object.drop_with(&mut vm.session.heap);
                return Err(self.unexpected(vm));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = match self.value(vm, depth + 1) {
                Ok(value) => value,
                Err(err) => {
                    object.drop_with(&mut vm.session.heap);
                    return Err(err);
                }
            };
            let key = match key.as_array_index() {
                Some(index) => PropertyKey::Index(index),
                None => PropertyKey::Atom(vm.session.interns.intern(key, &vm.session.heap.tracker)?),
            };
            vm.define_data(id, key, value);
            self.skip_whitespace();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(object);
                }
                _ => {
                    object.drop_with(&mut vm.session.heap);
                    return Err(self.unexpected(vm));
                }
            }
        }
    }
}

/// The first characters of the input, as V8 quotes them in its message.
fn preview(text: &str) -> String {
    let shown: String = text.chars().take(10).collect();
    if text.chars().count() > 10 {
        format!("{shown}...")
    } else {
        shown
    }
}
