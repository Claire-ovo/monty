//! `console.log` formatting: a port of the parts of Node's `util.inspect`
//! that plain values reach, so sandbox output matches the Node oracle byte
//! for byte — quoting, key display, `[Function: f]`, depth cut-off at 2,
//! the 80-column line break and the column grouping of long arrays.

use std::fmt::Write;

use super::{Vm, number::number_to_string};
use crate::{
    builtins::{date, regexp},
    error::RunResult,
    heap::{FunctionObject, HeapId, IterKind, JsObject, ObjectClass, PromiseState, PropertyKey, Slot},
    intern::{StaticAtom, WellKnownSymbol},
    value::JsValue,
};

/// Node's `breakLength` default.
const BREAK_LENGTH: usize = 80;
/// Node's `depth` default: objects nested deeper print as `[Object]`.
const MAX_DEPTH: usize = 2;
/// Node's `maxArrayLength` default.
const MAX_ARRAY_LENGTH: usize = 100;
/// Node's `maxStringLength` default.
const MAX_STRING_LENGTH: usize = 10_000;
/// Node's `compact` default.
const COMPACT: usize = 3;

/// State threaded through one `inspect` call.
struct Context {
    /// `%o`: show an array's `length` and go deeper (Node's `showHidden`).
    show_hidden: bool,
    /// Objects nested deeper than this print as `[Object]`.
    max_depth: usize,
    /// Objects being formatted, for cycle detection.
    seen: Vec<HeapId>,
    /// Objects found to be circular, numbered in discovery order.
    circular: Vec<HeapId>,
    indentation: usize,
    /// Depth of the most recently formatted object (Node's `ctx.currentDepth`).
    current_depth: usize,
}

impl Vm<'_> {
    /// Formats `console.log` arguments: strings raw, everything else inspected.
    pub fn format_log_args(&mut self, args: &[JsValue]) -> RunResult<String> {
        let mut parts = Vec::with_capacity(args.len());
        for arg in args {
            let text = match arg {
                JsValue::Atom(_) | JsValue::Str(_) => self.string_of(arg).expect("string").to_string_lossy(),
                other => self.inspect(other)?,
            };
            parts.push(text);
        }
        Ok(parts.join(" "))
    }

    /// `util.inspect(value)`.
    pub fn inspect(&mut self, value: &JsValue) -> RunResult<String> {
        self.inspect_with(value, false)
    }

    /// `util.inspect(value, { showHidden: true, depth: 4 })`, what `%o` prints.
    pub fn inspect_verbose(&mut self, value: &JsValue) -> RunResult<String> {
        self.inspect_with(value, true)
    }

    fn inspect_with(&mut self, value: &JsValue, show_hidden: bool) -> RunResult<String> {
        let mut ctx = Context {
            show_hidden,
            max_depth: if show_hidden { 4 } else { MAX_DEPTH },
            seen: Vec::new(),
            circular: Vec::new(),
            indentation: 0,
            current_depth: 0,
        };
        self.format_value(&mut ctx, value, 0, true)
    }

    /// A one-line description for error messages (`x is not a function`).
    pub fn inspect_short(&mut self, value: &JsValue) -> RunResult<String> {
        match value {
            JsValue::Ref(id) => Ok(if self.is_callable_id(*id) {
                self.function_base(*id)
            } else if self.is_array(value) {
                "[object Array]".to_owned()
            } else if matches!(self.object(*id).map(|o| &o.class), Some(ObjectClass::RegExp(_))) {
                "[object RegExp]".to_owned()
            } else if matches!(self.object(*id).map(|o| &o.class), Some(ObjectClass::Error)) {
                self.error_base(*id)?.lines().next().unwrap_or_default().to_owned()
            } else if let Some(name) = self.constructor_name(*id, false) {
                format!("#<{name}>")
            } else {
                let tag = self.string_tag(*id, true).unwrap_or_else(|| "Object".to_owned());
                format!("[object {tag}]")
            }),
            JsValue::Atom(_) | JsValue::Str(_) => Ok(self.string_of(value).expect("string").to_string_lossy()),
            other => self.inspect(other),
        }
    }

    /// [`inspect_short`](Self::inspect_short) for a value that was called:
    /// V8 quotes a string callee (`"abc" is not a function`).
    pub fn inspect_callee(&mut self, value: &JsValue) -> RunResult<String> {
        match value {
            JsValue::Atom(_) | JsValue::Str(_) => Ok(format!("\"{}\"", self.string_of(value).expect("string"))),
            other => self.inspect_short(other),
        }
    }

    fn format_value(&mut self, ctx: &mut Context, value: &JsValue, depth: usize, top: bool) -> RunResult<String> {
        Ok(match value {
            JsValue::Undefined | JsValue::Hole => "undefined".to_owned(),
            JsValue::Null => "null".to_owned(),
            JsValue::Bool(b) => b.to_string(),
            JsValue::Number(n) => format_number(*n),
            JsValue::Atom(_) | JsValue::Str(_) => {
                let string = self.string_of(value).expect("string").to_string_lossy();
                if top {
                    string
                } else {
                    format_string(&string, ctx.indentation)
                }
            }
            JsValue::Symbol(symbol) => self.key_display(PropertyKey::Symbol(*symbol)),
            JsValue::BigInt(_) => "[BigInt]".to_owned(),
            JsValue::Ref(id) => self.format_object(ctx, *id, depth)?,
        })
    }

    fn format_object(&mut self, ctx: &mut Context, id: HeapId, depth: usize) -> RunResult<String> {
        if ctx.seen.contains(&id) {
            let index = if let Some(index) = ctx.circular.iter().position(|c| *c == id) {
                index + 1
            } else {
                ctx.circular.push(id);
                ctx.circular.len()
            };
            return Ok(format!("[Circular *{index}]"));
        }
        let Some(object) = self.object(id) else {
            return Ok("[Object]".to_owned());
        };
        // the object's kind decides the braces, the base text and which keys show
        let (base, braces, is_array, is_error) = match &object.class {
            ObjectClass::Array => (String::new(), ["[", "]"], true, false),
            ObjectClass::Arguments => ("[Arguments]".to_owned(), ["[", "]"], true, false),
            ObjectClass::Function(_) => (self.function_base(id), ["{", "}"], false, false),
            ObjectClass::Error => (self.error_base(id)?, ["{", "}"], false, true),
            ObjectClass::BoxedBool(b) => (format!("[Boolean: {b}]"), ["{", "}"], false, false),
            ObjectClass::BoxedNumber(n) => (format!("[Number: {}]", format_number(*n)), ["{", "}"], false, false),
            ObjectClass::BoxedString(s) => (
                format!("[String: {}]", format_string(&s.to_string_lossy(), 0)),
                ["{", "}"],
                false,
                false,
            ),
            ObjectClass::BoxedSymbol(s) => (
                format!("[Symbol: {}]", self.key_display(PropertyKey::Symbol(*s))),
                ["{", "}"],
                false,
                false,
            ),
            ObjectClass::ArrayIterator { .. } => ("Object [Array Iterator]".to_owned(), ["{", "}"], false, false),
            ObjectClass::Map(data) => (format!("Map({})", data.live), ["{", "}"], false, false),
            ObjectClass::Set(data) => (format!("Set({})", data.live), ["{", "}"], false, false),
            ObjectClass::WeakMap(_) => ("WeakMap".to_owned(), ["{", "}"], false, false),
            ObjectClass::WeakSet(_) => ("WeakSet".to_owned(), ["{", "}"], false, false),
            ObjectClass::MapIterator { map, kind, .. } => {
                let is_set = matches!(self.object(*map).map(|o| &o.class), Some(ObjectClass::Set(_)));
                let name = match (is_set, kind) {
                    (false, IterKind::Entries) => "[Map Entries]",
                    (false, _) => "[Map Iterator]",
                    (true, IterKind::Entries) => "[Set Entries]",
                    (true, _) => "[Set Iterator]",
                };
                (name.to_owned(), ["{", "}"], false, false)
            }
            ObjectClass::StringIterator { .. } => ("Object [String Iterator]".to_owned(), ["{", "}"], false, false),
            ObjectClass::RegExp(data) => (regexp::to_string(data), ["{", "}"], false, false),
            ObjectClass::RegExpStringIterator { .. } => {
                ("Object [RegExp String Iterator]".to_owned(), ["{", "}"], false, false)
            }
            ObjectClass::Generator(_) | ObjectClass::Coroutine(_) => {
                (self.ordinary_prefix(id), ["{", "}"], false, false)
            }
            ObjectClass::Promise(_) => (self.ordinary_prefix(id), ["{", "}"], false, false),
            ObjectClass::Date(t) => (date::inspect_value(*t), ["{", "}"], false, false),
            ObjectClass::Ordinary => (self.ordinary_prefix(id), ["{", "}"], false, false),
        };
        // a regexp or date without extra properties prints whole at any depth, like a primitive
        let bare_regexp = self.object(id).is_some_and(|object| {
            matches!(object.class, ObjectClass::RegExp(_) | ObjectClass::Date(_))
                && !object.props.values().any(|slot| slot.flags().enumerable())
        });
        if bare_regexp {
            return Ok(base);
        }
        if depth > ctx.max_depth {
            return Ok(if is_array {
                "[Array]".to_owned()
            } else if base.is_empty() {
                "[Object]".to_owned()
            } else if is_error {
                format!("[{}]", base.lines().next().unwrap_or_default())
            } else {
                format!("[{base}]")
            });
        }
        ctx.seen.push(id);
        ctx.current_depth = depth;
        let mut output = Vec::new();
        let mut shown_keys: Vec<PropertyKey> = Vec::new();
        self.format_collection(ctx, id, depth, &mut output)?;
        self.format_promise_state(ctx, id, depth, &mut output)?;
        if is_array {
            let len = self.object(id).map_or(0, |o| o.elements.len());
            let shown = len.min(MAX_ARRAY_LENGTH);
            let mut holes = 0usize;
            for index in 0..shown {
                self.tracker().check_memory_time_every(index)?;
                let element = self
                    .object(id)
                    .and_then(|o| o.elements.get(index))
                    .map_or(JsValue::Hole, |v| v.clone_with_heap(&self.session.heap));
                if element == JsValue::Hole {
                    holes += 1;
                    continue;
                }
                if holes > 0 {
                    output.push(format!("<{holes} empty item{}>", if holes == 1 { "" } else { "s" }));
                    holes = 0;
                }
                ctx.indentation += 2;
                let text = self.format_value(ctx, &element, depth + 1, false);
                ctx.indentation -= 2;
                element.drop_with(&mut self.session.heap);
                output.push(text?);
            }
            if holes > 0 {
                output.push(format!("<{holes} empty item{}>", if holes == 1 { "" } else { "s" }));
            }
            if len > shown {
                let remaining = len - shown;
                output.push(format!(
                    "... {remaining} more item{}",
                    if remaining == 1 { "" } else { "s" }
                ));
            }
            if ctx.show_hidden {
                output.push(format!("[length]: {len}"));
            }
        }
        // named properties (enumerable only), then symbols
        let keys = self.own_keys(id, false);
        for (seen, key) in keys.into_iter().enumerate() {
            // the output grows per key, outside the dispatch loop's checkpoint
            self.tracker().check_memory_time_every(seen)?;
            if matches!(key, PropertyKey::Index(_)) && is_array {
                continue;
            }
            let slot = self.object(id).and_then(|o| o.props.get(&key));
            let Some(slot) = slot else {
                continue;
            };
            if !slot.flags().enumerable() {
                continue;
            }
            shown_keys.push(key);
            let entry = match slot {
                Slot::Data { value, .. } => {
                    let value = value.clone_with_heap(&self.session.heap);
                    ctx.indentation += 2;
                    let text = self.format_value(ctx, &value, depth + 1, false);
                    ctx.indentation -= 2;
                    value.drop_with(&mut self.session.heap);
                    text?
                }
                Slot::Accessor { getter, setter, .. } => match (getter.is_nullish(), setter.is_nullish()) {
                    (false, false) => "[Getter/Setter]".to_owned(),
                    (false, true) => "[Getter]".to_owned(),
                    _ => "[Setter]".to_owned(),
                },
            };
            let name = self.format_key(key);
            output.push(format!("{name}: {entry}"));
        }
        ctx.seen.pop();
        let circular_index = ctx.circular.iter().position(|c| *c == id).map(|i| i + 1);
        let mut base = base;
        if let Some(index) = circular_index {
            base = if base.is_empty() {
                format!("<ref *{index}>")
            } else {
                format!("<ref *{index}> {base}")
            };
        }
        let is_weak = matches!(
            self.object(id).map(|o| &o.class),
            Some(ObjectClass::WeakMap(_) | ObjectClass::WeakSet(_))
        );
        if output.is_empty() && is_weak {
            return Ok(format!("{base} {{ <items unknown> }}"));
        }
        if output.is_empty() {
            return Ok(if is_array {
                if base.is_empty() {
                    "[]".to_owned()
                } else {
                    format!("{base} []")
                }
            } else if is_error {
                base
            } else if base.is_empty() {
                "{}".to_owned()
            } else if matches!(
                self.object(id).map(|o| &o.class),
                Some(
                    ObjectClass::Function(_)
                        | ObjectClass::BoxedBool(_)
                        | ObjectClass::BoxedNumber(_)
                        | ObjectClass::BoxedString(_)
                        | ObjectClass::BoxedSymbol(_)
                )
            ) {
                base
            } else {
                format!("{base} {{}}")
            });
        }
        if is_error {
            // Node prints the stack, then the extra properties in braces
            let joined = reduce_to_single_string(ctx, output, &base, braces, depth, true);
            return Ok(joined);
        }
        Ok(reduce_to_single_string(ctx, output, &base, braces, depth, !is_array))
    }

    /// A promise's state as Node shows it: `<pending>`, the value, or `<rejected> reason`.
    fn format_promise_state(
        &mut self,
        ctx: &mut Context,
        id: HeapId,
        depth: usize,
        output: &mut Vec<String>,
    ) -> RunResult<()> {
        let state = match self.object(id).map(|o| &o.class) {
            Some(ObjectClass::Promise(data)) => match &data.state {
                PromiseState::Pending(_) => None,
                PromiseState::Fulfilled(value) => Some((false, value.clone_with_heap(&self.session.heap))),
                PromiseState::Rejected(value) => Some((true, value.clone_with_heap(&self.session.heap))),
            },
            _ => return Ok(()),
        };
        match state {
            None => output.push("<pending>".to_owned()),
            Some((rejected, value)) => {
                ctx.indentation += 2;
                let text = self.format_value(ctx, &value, depth + 1, false);
                ctx.indentation -= 2;
                value.drop_with(&mut self.session.heap);
                let text = text?;
                output.push(if rejected { format!("<rejected> {text}") } else { text });
            }
        }
        Ok(())
    }

    /// The entries of a `Map`, `Set` or their iterators, as `key => value`,
    /// values, or `[ key, value ]` pairs.
    fn format_collection(
        &mut self,
        ctx: &mut Context,
        id: HeapId,
        depth: usize,
        output: &mut Vec<String>,
    ) -> RunResult<()> {
        let (map, kind, is_set, start) = match self.object(id).map(|o| &o.class) {
            Some(ObjectClass::Map(_)) => (id, IterKind::Entries, false, 0),
            Some(ObjectClass::Set(_)) => (id, IterKind::Keys, true, 0),
            Some(ObjectClass::MapIterator { map, kind, index }) => {
                let is_set = matches!(self.object(*map).map(|o| &o.class), Some(ObjectClass::Set(_)));
                (*map, *kind, is_set, *index as usize)
            }
            _ => return Ok(()),
        };
        let entries: Vec<(JsValue, JsValue)> = match self.object(map).map(|o| &o.class) {
            Some(ObjectClass::Map(data) | ObjectClass::Set(data)) => data.entries[start.min(data.entries.len())..]
                .iter()
                .flatten()
                .map(|entry| {
                    (
                        entry.key.clone_with_heap(&self.session.heap),
                        entry.value.clone_with_heap(&self.session.heap),
                    )
                })
                .collect(),
            _ => Vec::new(),
        };
        let total = entries.len();
        for (shown, (key, value)) in entries.into_iter().enumerate() {
            self.tracker().check_memory_time_every(shown)?;
            if shown >= MAX_ARRAY_LENGTH {
                key.drop_with(&mut self.session.heap);
                value.drop_with(&mut self.session.heap);
                continue;
            }
            ctx.indentation += 2;
            let text = match kind {
                IterKind::Keys => self.format_value(ctx, &key, depth + 1, false),
                IterKind::Values => self.format_value(ctx, &value, depth + 1, false),
                IterKind::Entries => {
                    let key_text = self.format_value(ctx, &key, depth + 1, false);
                    let value_text = if is_set {
                        self.format_value(ctx, &key, depth + 1, false)
                    } else {
                        self.format_value(ctx, &value, depth + 1, false)
                    };
                    match (key_text, value_text) {
                        (Ok(k), Ok(v))
                            if is_set
                                || matches!(
                                    self.object(id).map(|o| &o.class),
                                    Some(ObjectClass::MapIterator { .. })
                                ) =>
                        {
                            Ok(format!("[ {k}, {v} ]"))
                        }
                        (Ok(k), Ok(v)) => Ok(format!("{k} => {v}")),
                        (Err(err), _) | (_, Err(err)) => Err(err),
                    }
                }
            };
            ctx.indentation -= 2;
            key.drop_with(&mut self.session.heap);
            value.drop_with(&mut self.session.heap);
            output.push(text?);
        }
        if total > MAX_ARRAY_LENGTH {
            let remaining = total - MAX_ARRAY_LENGTH;
            output.push(format!(
                "... {remaining} more item{}",
                if remaining == 1 { "" } else { "s" }
            ));
        }
        Ok(())
    }

    /// The `[Function: name]` form of a function object, or `[class X extends Y]`.
    fn function_base(&self, id: HeapId) -> String {
        let Some(object) = self.object(id) else {
            return "[Function]".to_owned();
        };
        let name = self.own_name(object);
        let is_class = matches!(&object.class, ObjectClass::Function(FunctionObject::Closure { func, .. }) if self.session.interns.function(*func).kind.is_class_constructor());
        if is_class {
            let parent = object
                .proto
                .filter(|proto| *proto != self.session.realm.function_prototype)
                .and_then(|proto| self.object(proto))
                .and_then(|parent| self.own_name(parent))
                .filter(|name| !name.is_empty());
            return match (name, parent) {
                (Some(name), Some(parent)) if !name.is_empty() => format!("[class {name} extends {parent}]"),
                (Some(name), None) if !name.is_empty() => format!("[class {name}]"),
                (_, Some(parent)) => format!("[class (anonymous) extends {parent}]"),
                _ => "[class (anonymous)]".to_owned(),
            };
        }
        let bound = matches!(object.class, ObjectClass::Function(FunctionObject::Bound { .. }));
        let kind = match &object.class {
            ObjectClass::Function(FunctionObject::Closure { func, .. }) => {
                Some(self.session.interns.function(*func).kind)
            }
            _ => None,
        };
        let special = match kind {
            Some(kind) if kind.is_generator() => Some("GeneratorFunction"),
            Some(kind) if kind.is_async() => Some("AsyncFunction"),
            _ => None,
        };
        if let Some(label) = special {
            return match name {
                Some(name) if !name.is_empty() => format!("[{label}: {name}]"),
                _ => format!("[{label} (anonymous)]"),
            };
        }
        match name {
            Some(name) if !name.is_empty() => format!("[Function: {name}]"),
            _ if bound => "[Function: bound]".to_owned(),
            _ => "[Function (anonymous)]".to_owned(),
        }
    }

    /// A function object's own `name` data property.
    fn own_name(&self, object: &JsObject) -> Option<String> {
        match object.props.get(&PropertyKey::Atom(StaticAtom::Name.id())) {
            Some(Slot::Data { value, .. }) => self.string_of(value).map(|s| s.to_string_lossy()),
            _ => None,
        }
    }

    /// An error prints its `stack`.
    fn error_base(&mut self, id: HeapId) -> RunResult<String> {
        let value = JsValue::Ref(id);
        let stack = self.get_atom(&value, StaticAtom::Stack.id())?;
        let text = if stack.is_string() {
            self.string_of(&stack).expect("string").to_string_lossy()
        } else {
            let name = self.get_atom(&value, StaticAtom::Name.id())?;
            let message = self.get_atom(&value, StaticAtom::Message.id())?;
            let name_text = self.coerce_string(&name)?.to_string_lossy();
            let message_text = self.coerce_string(&message)?.to_string_lossy();
            name.drop_with(&mut self.session.heap);
            message.drop_with(&mut self.session.heap);
            if message_text.is_empty() {
                format!("[{name_text}]")
            } else {
                format!("[{name_text}: {message_text}]")
            }
        };
        stack.drop_with(&mut self.session.heap);
        Ok(text)
    }

    /// Objects whose prototype is not `Object.prototype` show their
    /// constructor's name: `Foo { … }` or `[Object: null prototype] { … }`.
    fn ordinary_prefix(&self, id: HeapId) -> String {
        let Some(object) = self.object(id) else {
            return String::new();
        };
        if object.proto.is_none() {
            return "[Object: null prototype]".to_owned();
        }
        let name = self.constructor_name(id, true).unwrap_or_default();
        // Node shows an inherited (or hidden own) `Symbol.toStringTag` that differs from the constructor
        match self.string_tag(id, false) {
            Some(tag) if tag != name => {
                let name = if name.is_empty() { "Object" } else { &name };
                format!("{name} [{tag}]")
            }
            _ if name == "Object" => String::new(),
            _ => name,
        }
    }

    /// The name of the nearest `constructor` along the prototype chain (own
    /// property included) that is a function with a non-empty name. Node keeps
    /// looking past a `constructor` that is not a function (`skip_non_functions`);
    /// V8's error messages stop at the first one.
    fn constructor_name(&self, id: HeapId, skip_non_functions: bool) -> Option<String> {
        let mut current = Some(id);
        while let Some(current_id) = current {
            let object = self.object(current_id)?;
            if let Some(Slot::Data { value, .. }) = object.props.get(&PropertyKey::Atom(StaticAtom::Constructor.id())) {
                let function = value.heap_id().filter(|ctor| self.is_callable_id(*ctor));
                if function.is_some() || !skip_non_functions {
                    return function
                        .and_then(|ctor| self.object(ctor))
                        .and_then(|c| self.own_name(c))
                        .filter(|name| !name.is_empty());
                }
            }
            current = object.proto;
        }
        None
    }

    /// A string `Symbol.toStringTag` found on the object or its prototypes;
    /// an own enumerable tag is ignored unless `own_enumerable` is set.
    fn string_tag(&self, id: HeapId, own_enumerable: bool) -> Option<String> {
        let key = PropertyKey::Symbol(WellKnownSymbol::ToStringTag.id());
        let mut current = Some(id);
        let mut own = true;
        while let Some(current_id) = current {
            let object = self.object(current_id)?;
            if let Some(Slot::Data { value, flags }) = object.props.get(&key) {
                let visible = own_enumerable || !own || !flags.enumerable();
                return match self.string_of(value) {
                    Some(tag) if visible && !tag.is_empty() => Some(tag.to_string_lossy()),
                    _ => None,
                };
            }
            own = false;
            current = object.proto;
        }
        None
    }

    /// A key as a property label: bare identifiers, quoted strings, `[Symbol(x)]`.
    fn format_key(&self, key: PropertyKey) -> String {
        match key {
            PropertyKey::Index(index) => format!("'{index}'"),
            PropertyKey::Atom(atom) => {
                let text = self.session.interns.atom(atom).to_string_lossy();
                if is_identifier(&text) {
                    text
                } else {
                    format_string(&text, 0)
                }
            }
            PropertyKey::Symbol(symbol) => self.key_display(PropertyKey::Symbol(symbol)),
        }
    }
}

/// Joins an object's entries on one line when they fit, else one per line
/// (`reduceToSingleString` in Node, with array column grouping).
fn reduce_to_single_string(
    ctx: &Context,
    mut output: Vec<String>,
    base: &str,
    braces: [&str; 2],
    depth: usize,
    is_object: bool,
) -> String {
    let entries = output.len();
    if !is_object && entries > 6 {
        output = group_array_elements(ctx, output);
    }
    if ctx.current_depth - depth < COMPACT && entries == output.len() {
        let start = output.len() + ctx.indentation + braces[0].len() + base.len() + 10;
        if is_below_break_length(&output, start, base) {
            let joined = output.join(", ");
            if !joined.contains('\n') {
                let prefix = if base.is_empty() {
                    String::new()
                } else {
                    format!("{base} ")
                };
                return format!("{prefix}{} {joined} {}", braces[0], braces[1]);
            }
        }
    }
    let indentation = format!("\n{}", " ".repeat(ctx.indentation));
    let prefix = if base.is_empty() {
        String::new()
    } else {
        format!("{base} ")
    };
    format!(
        "{prefix}{}{indentation}  {}{indentation}{}",
        braces[0],
        output.join(&format!(",{indentation}  ")),
        braces[1]
    )
}

fn is_below_break_length(output: &[String], start: usize, base: &str) -> bool {
    let mut total = output.len() + start;
    if total + output.len() > BREAK_LENGTH {
        return false;
    }
    for entry in output {
        total += entry.chars().count();
        if total > BREAK_LENGTH {
            return false;
        }
    }
    base.is_empty() || !base.contains('\n')
}

/// Node's `groupArrayElements`: lays short entries out in aligned columns.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a port of Node's floating-point column heuristics"
)]
fn group_array_elements(ctx: &Context, output: Vec<String>) -> Vec<String> {
    let mut total_length = 0usize;
    let mut max_length = 0usize;
    let mut output_length = output.len();
    let has_more = output
        .last()
        .is_some_and(|last| last.starts_with("... ") && last.ends_with("more items") || last.ends_with("more item"));
    if has_more {
        output_length -= 1;
    }
    let separator_space = 2;
    let data_len: Vec<usize> = output[..output_length].iter().map(|s| s.chars().count()).collect();
    for len in &data_len {
        total_length += len + separator_space;
        max_length = max_length.max(*len);
    }
    let actual_max = max_length + separator_space;
    if actual_max * 3 + ctx.indentation < BREAK_LENGTH
        && (total_length as f64 / actual_max as f64 > 5.0 || max_length <= 6)
    {
        let approx_char_heights = 2.5;
        let average_bias = (actual_max as f64 - total_length as f64 / output.len() as f64).sqrt();
        let biased_max = (actual_max as f64 - 3.0 - average_bias).max(1.0);
        let columns = ((approx_char_heights * biased_max * output_length as f64).sqrt() / biased_max)
            .round()
            .min(((BREAK_LENGTH - ctx.indentation) / actual_max) as f64)
            .min((COMPACT * 4) as f64)
            .min(15.0) as usize;
        if columns <= 1 {
            return output;
        }
        let mut max_line_length = Vec::with_capacity(columns);
        for i in 0..columns {
            let mut line_length = 0;
            let mut j = i;
            while j < output_length {
                line_length = line_length.max(data_len[j]);
                j += columns;
            }
            max_line_length.push(line_length + separator_space);
        }
        // numbers are right-aligned, everything else left-aligned
        let pad_start = output[..output_length].iter().all(|s| is_numeric_entry(s));
        let mut grouped = Vec::new();
        let mut i = 0;
        while i < output_length {
            let max = (i + columns).min(output_length);
            let mut line = String::new();
            let mut j = i;
            while j < max - 1 {
                let cell = format!("{}, ", output[j]);
                let padding = max_line_length[j - i];
                line.push_str(&pad(&cell, padding, pad_start));
                j += 1;
            }
            if pad_start {
                let padding = max_line_length[j - i] - separator_space;
                line.push_str(&pad(&output[j], padding, true));
            } else {
                line.push_str(&output[j]);
            }
            grouped.push(line);
            i += columns;
        }
        if has_more {
            grouped.push(output[output_length].clone());
        }
        return grouped;
    }
    output
}

/// Whether an entry is a number as `format_number` prints it.
fn is_numeric_entry(text: &str) -> bool {
    text == "NaN" || text == "Infinity" || text == "-Infinity" || text == "-0" || text.parse::<f64>().is_ok()
}

fn pad(text: &str, width: usize, start: bool) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_owned()
    } else if start {
        format!("{}{text}", " ".repeat(width - len))
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// A number as `util.inspect` shows it (`-0` keeps its sign).
fn format_number(n: f64) -> String {
    if n == 0.0 && n.is_sign_negative() {
        "-0".to_owned()
    } else {
        number_to_string(n)
    }
}

/// A string as `util.inspect` quotes it: single quotes unless the string
/// contains single quotes and no double quotes (then double), or both (then
/// backticks); long multi-line strings are split into `+`-joined lines.
fn format_string(text: &str, indentation: usize) -> String {
    let (truncated, remaining) = if text.chars().count() > MAX_STRING_LENGTH {
        let cut: String = text.chars().take(MAX_STRING_LENGTH).collect();
        let remaining = text.chars().count() - MAX_STRING_LENGTH;
        (cut, remaining)
    } else {
        (text.to_owned(), 0)
    };
    let quote = if !truncated.contains('\'') {
        '\''
    } else if !truncated.contains('"') {
        '"'
    } else if !truncated.contains('`') && !truncated.contains("${") {
        '`'
    } else {
        '\''
    };
    let trailer = if remaining > 0 {
        format!(
            "... {remaining} more character{}",
            if remaining == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let escape = |segment: &str| {
        let mut out = String::with_capacity(segment.len() + 2);
        out.push(quote);
        for c in segment.chars() {
            match c {
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                '\u{b}' => out.push_str("\\v"),
                '\\' => out.push_str("\\\\"),
                c if c == quote => {
                    out.push('\\');
                    out.push(c);
                }
                c if (c as u32) < 0x20 || c == '\u{7f}' => {
                    let _ = write!(out, "\\x{:02X}", c as u32);
                }
                c => out.push(c),
            }
        }
        out.push(quote);
        out
    };
    // multi-line strings inside objects are split so each line is its own literal
    if indentation > 0 && truncated.len() > 16 && truncated.contains('\n') {
        let mut lines: Vec<String> = Vec::new();
        let mut rest = truncated.as_str();
        while let Some(index) = rest.find('\n') {
            lines.push(escape(&rest[..=index]));
            rest = &rest[index + 1..];
        }
        if !rest.is_empty() {
            lines.push(escape(rest));
        }
        if lines.len() > 1 {
            let joiner = format!(" +\n{}", " ".repeat(indentation + 2));
            return format!("{}{trailer}", lines.join(&joiner));
        }
    }
    format!("{}{trailer}", escape(&truncated))
}

/// Whether a key prints unquoted.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
