//! JSX as a templating language: the `jsx` factory every element lowers to,
//! `Fragment`, and `renderToString`.
//!
//! An element evaluates eagerly to a plain tree the host can consume:
//! `{ type, props, children }`, where `type` is the tag, `props` the
//! attributes (minus `children`) and `children` a flat array of nodes and
//! strings — nullish and boolean children are dropped, numbers become
//! strings, nested arrays flatten. A component (a function type) is called
//! with its props, `children` included, and its return value stands in for
//! the element; a fragment evaluates to its children array. A component may be
//! `async`: a thenable anywhere in the children makes the element itself a
//! promise of the node, settled once every child has. `renderToString` walks
//! a tree into HTML with the attribute conventions React users expect
//! (`className`, `htmlFor`, `style` objects, `dangerouslySetInnerHTML`), and
//! returns a promise when handed one.

use std::slice;

use super::{NativeFn, arg, install_function, install_value, key, plain_object};
use crate::{
    error::RunResult,
    heap::{FunctionObject, Handler, HeapId, PropFlags, PropertyKey, Target},
    intern::StaticAtom,
    string::JsString,
    value::JsValue,
    vm::{Vm, number::number_to_string},
};

/// Nesting beyond this is treated as a runaway structure, as `JSON.stringify` does.
const MAX_TREE_DEPTH: usize = 512;

/// HTML elements that take no closing tag and no children.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param", "source", "track",
    "wbr",
];

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let jsx = install_function(vm, global, NativeFn::Jsx);
    vm.session.heap.inc_ref(jsx);
    vm.session.realm.jsx_function = jsx;
    // `Fragment` is only compared by identity, so an empty frozen object serves
    let fragment = plain_object(vm);
    if let Some(object) = vm.object_mut(fragment) {
        object.extensible = false;
    }
    vm.session.heap.inc_ref(fragment);
    vm.session.realm.jsx_fragment = fragment;
    install_value(vm, global, "Fragment", JsValue::Ref(fragment), PropFlags::HIDDEN);
    install_function(vm, global, NativeFn::RenderToString);
}

/// `jsx(type, props, ...children)`: an element node, a component's result, or
/// a fragment's children.
pub(crate) fn jsx(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let element_type = arg(args, 0);
    let props = arg(args, 1);
    let children = args.get(2..).unwrap_or_default();
    if element_type.heap_id() == Some(vm.session.realm.jsx_fragment) {
        let (flat, pending) = normalize_children(vm, clone_all(vm, children))?;
        let array = vm.new_array(flat)?;
        finish_children(vm, JsValue::Undefined, array, pending)
    } else if element_type.is_string() {
        element(vm, element_type, props, children)
    } else if vm.is_callable(element_type) {
        component(vm, element_type, props, children)
    } else {
        let got = type_name(vm, element_type);
        Err(vm.type_error(&format!(
            "Element type is invalid: expected a string or a function but got: {got}"
        )))
    }
}

/// An intrinsic element: the node object, settled now or once its children are.
fn element(vm: &mut Vm<'_>, element_type: &JsValue, props: &JsValue, children: &[JsValue]) -> RunResult<JsValue> {
    let node = vm.new_object();
    let node_id = node.heap_id().expect("object");
    let type_key = key(vm, "type");
    vm.define_data(node_id, type_key, element_type.clone_with_heap(&vm.session.heap));
    let copied = vm.new_object();
    let copied_id = copied.heap_id().expect("object");
    let result = vm.copy_data_properties(copied_id, props);
    let props_key = key(vm, "props");
    vm.define_data(node_id, props_key, copied);
    if let Err(err) = result {
        node.drop_with(&mut vm.session.heap);
        return Err(err);
    }
    // `children={x}` is overridden by children written between the tags
    let children_key = key(vm, "children");
    let copied = JsValue::Ref(copied_id);
    let raw = if children.is_empty() {
        if vm.has_own_id(copied_id, children_key) {
            let explicit = vm.get(&copied, children_key)?;
            vm.delete(&copied, children_key)?;
            vec![explicit]
        } else {
            Vec::new()
        }
    } else {
        vm.delete(&copied, children_key)?;
        clone_all(vm, children)
    };
    let normalized = normalize_children(vm, raw);
    let (flat, pending) = match normalized {
        Ok(normalized) => normalized,
        Err(err) => {
            node.drop_with(&mut vm.session.heap);
            return Err(err);
        }
    };
    let array = match vm.new_array(flat) {
        Ok(array) => array,
        Err(err) => {
            node.drop_with(&mut vm.session.heap);
            return Err(err);
        }
    };
    vm.define_data(node_id, children_key, array.clone_with_heap(&vm.session.heap));
    finish_children(vm, node, array, pending)
}

/// A component called from a native (`jsx(Comp, …)` by hand): its result is
/// the element. Lowered JSX takes `Opcode::JsxElement` instead, which calls
/// the component from bytecode so that host calls inside it can suspend.
fn component(vm: &mut Vm<'_>, component: &JsValue, props: &JsValue, children: &[JsValue]) -> RunResult<JsValue> {
    let passed = component_props(vm, props, children)?;
    let callee = component.clone_with_heap(&vm.session.heap);
    vm.call(callee, JsValue::Undefined, vec![passed])
}

/// The props object a component receives: the attributes plus `children` the
/// way React passes them (absent, the single child, or the array).
pub(crate) fn component_props(vm: &mut Vm<'_>, props: &JsValue, children: &[JsValue]) -> RunResult<JsValue> {
    let passed = vm.new_object();
    let passed_id = passed.heap_id().expect("object");
    if let Err(err) = vm.copy_data_properties(passed_id, props) {
        passed.drop_with(&mut vm.session.heap);
        return Err(err);
    }
    let children_key = key(vm, "children");
    match children {
        [] => {}
        [only] => {
            let only = only.clone_with_heap(&vm.session.heap);
            vm.define_data(passed_id, children_key, only);
        }
        many => {
            let items = clone_all(vm, many);
            match vm.new_array(items) {
                Ok(array) => vm.define_data(passed_id, children_key, array),
                Err(err) => {
                    passed.drop_with(&mut vm.session.heap);
                    return Err(err);
                }
            }
        }
    }
    Ok(passed)
}

/// Hands back `owner` (a node, or `undefined` for a fragment's bare array)
/// once `array` holds settled children: at once, or through a promise that
/// re-normalizes the settled values (consumes both).
fn finish_children(vm: &mut Vm<'_>, owner: JsValue, array: JsValue, pending: bool) -> RunResult<JsValue> {
    if !pending {
        return Ok(match owner {
            JsValue::Undefined => array,
            node => {
                array.drop_with(&mut vm.session.heap);
                node
            }
        });
    }
    // `Promise.all` waits for every child; `settle` then rebuilds the array
    let all = super::promise::all(vm, &JsValue::Undefined, slice::from_ref(&array), false);
    array.drop_with(&mut vm.session.heap);
    let all = match all {
        Ok(all) => all,
        Err(err) => {
            owner.drop_with(&mut vm.session.heap);
            return Err(err);
        }
    };
    let all_id = all.heap_id().expect("promise");
    let result = vm.new_promise();
    let settle = vm.bound_native(NativeFn::JsxSettle, owner, Vec::new());
    vm.session.heap.inc_ref(result);
    vm.promise_then(all_id, Handler::Call(settle), Handler::Thrower, Target::Promise(result));
    all.drop_with(&mut vm.session.heap);
    Ok(JsValue::Ref(result))
}

/// The children of an element or fragment have settled: `this` is the owner
/// (the node, or `undefined` for a fragment), the argument the values. Settled
/// components may have produced more thenables, so this may wait again.
pub(super) fn settle(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let values = arg(args, 0).clone_with_heap(&vm.session.heap);
    let items = vm.array_to_vec(values)?;
    let (flat, pending) = normalize_children(vm, items)?;
    let array = vm.new_array(flat)?;
    let owner = this.clone_with_heap(&vm.session.heap);
    if let JsValue::Ref(_) = &owner {
        let children_key = key(vm, "children");
        if let Err(err) = vm.set(&owner, children_key, array.clone_with_heap(&vm.session.heap)) {
            vm.drop_all(owner, vec![array]);
            return Err(err);
        }
    }
    finish_children(vm, owner, array, pending)
}

/// Flattens `items` (consumed) into the children of a node: arrays spread,
/// nullish and boolean values vanish, numbers become strings. Reports whether
/// a thenable is among them.
fn normalize_children(vm: &mut Vm<'_>, items: Vec<JsValue>) -> RunResult<(Vec<JsValue>, bool)> {
    let mut out = Vec::new();
    let mut pending = false;
    match normalize_into(vm, items, &mut out, &mut pending, 0) {
        Ok(()) => Ok((out, pending)),
        Err(err) => {
            for value in out {
                value.drop_with(&mut vm.session.heap);
            }
            Err(err)
        }
    }
}

fn normalize_into(
    vm: &mut Vm<'_>,
    items: Vec<JsValue>,
    out: &mut Vec<JsValue>,
    pending: &mut bool,
    depth: usize,
) -> RunResult<()> {
    let mut items = items.into_iter();
    while let Some(item) = items.next() {
        let step = normalize_one(vm, item, out, pending, depth);
        if let Err(err) = step {
            for rest in items {
                rest.drop_with(&mut vm.session.heap);
            }
            return Err(err);
        }
    }
    Ok(())
}

/// One child (consumed): appended to `out`, spread into it, or dropped.
fn normalize_one(
    vm: &mut Vm<'_>,
    item: JsValue,
    out: &mut Vec<JsValue>,
    pending: &mut bool,
    depth: usize,
) -> RunResult<()> {
    if depth > MAX_TREE_DEPTH {
        item.drop_with(&mut vm.session.heap);
        return Err(vm.range_error("Maximum call stack size exceeded"));
    }
    vm.tracker().check_memory_time_every(out.len())?;
    match item {
        JsValue::Undefined | JsValue::Null | JsValue::Hole | JsValue::Bool(_) => Ok(()),
        JsValue::Number(number) => {
            let text = vm.alloc_string(JsString::from_str(&number_to_string(number)))?;
            out.push(text);
            Ok(())
        }
        JsValue::Atom(_) | JsValue::Str(_) => {
            out.push(item);
            Ok(())
        }
        JsValue::BigInt(_) => {
            let text = vm.coerce_string(&item)?;
            item.drop_with(&mut vm.session.heap);
            out.push(vm.alloc_string(text)?);
            Ok(())
        }
        JsValue::Symbol(_) => Err(vm.type_error("Symbols are not valid as a JSX child")),
        JsValue::Ref(id) => {
            if vm.is_array(&item) {
                let nested = vm.array_to_vec(item)?;
                normalize_into(vm, nested, out, pending, depth + 1)
            } else {
                let thenable = is_thenable(vm, &item);
                match thenable {
                    Ok(true) => {
                        *pending = true;
                        out.push(item);
                        Ok(())
                    }
                    Ok(false) if is_node(vm, id) => {
                        out.push(item);
                        Ok(())
                    }
                    Ok(false) => {
                        item.drop_with(&mut vm.session.heap);
                        Err(vm.type_error("Objects are not valid as a JSX child"))
                    }
                    Err(err) => {
                        item.drop_with(&mut vm.session.heap);
                        Err(err)
                    }
                }
            }
        }
    }
}

/// Whether `value` has a callable `then` (a promise, or anything posing as one).
fn is_thenable(vm: &mut Vm<'_>, value: &JsValue) -> RunResult<bool> {
    let then = vm.get_atom(value, StaticAtom::Then.id())?;
    let callable = vm.is_callable(&then);
    then.drop_with(&mut vm.session.heap);
    Ok(callable)
}

/// Whether an object is an element node: it carries both `type` and `children`.
fn is_node(vm: &mut Vm<'_>, id: HeapId) -> bool {
    let type_key = key(vm, "type");
    let children_key = key(vm, "children");
    vm.has_own_id(id, type_key) && vm.has_own_id(id, children_key)
}

fn clone_all(vm: &Vm<'_>, values: &[JsValue]) -> Vec<JsValue> {
    values
        .iter()
        .map(|value| value.clone_with_heap(&vm.session.heap))
        .collect()
}

/// `typeof`-style name for an error message (`null` named, not `object`).
fn type_name(vm: &Vm<'_>, value: &JsValue) -> &'static str {
    match value {
        JsValue::Undefined | JsValue::Hole => "undefined",
        JsValue::Null => "null",
        JsValue::Bool(_) => "boolean",
        JsValue::Number(_) => "number",
        JsValue::Atom(_) | JsValue::Str(_) => "string",
        JsValue::Symbol(_) => "symbol",
        JsValue::BigInt(_) => "bigint",
        JsValue::Ref(_) if vm.is_callable(value) => "function",
        JsValue::Ref(_) => "object",
    }
}

// === renderToString ===

/// `renderToString(tree)`: the HTML of a tree, or a promise of it when the
/// tree is still settling.
pub(super) fn render_to_string(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let tree = arg(args, 0);
    if matches!(tree, JsValue::Ref(_)) && is_thenable(vm, tree)? {
        return render_later(vm, tree);
    }
    let mut renderer = Renderer {
        out: Vec::new(),
        visited: 0,
    };
    renderer.render(vm, tree, 0)?;
    vm.alloc_string(JsString::from_units(&renderer.out))
}

/// `tree.then(renderToString)`, as a fresh promise.
fn render_later(vm: &mut Vm<'_>, tree: &JsValue) -> RunResult<JsValue> {
    let promise = vm.promise_resolve(tree.clone_with_heap(&vm.session.heap))?;
    let result = vm.new_promise();
    let name = vm.session.interns.intern_str(NativeFn::RenderToString.name());
    let render = vm.new_function_object(FunctionObject::Native(NativeFn::RenderToString), name, 1);
    vm.session.heap.inc_ref(result);
    vm.promise_then(
        promise,
        Handler::Call(JsValue::Ref(render)),
        Handler::Thrower,
        Target::Promise(result),
    );
    vm.session.heap.dec_ref(promise);
    Ok(JsValue::Ref(result))
}

/// The HTML being built, as UTF-16 code units like every other string.
struct Renderer {
    out: Vec<u16>,
    /// Values rendered so far, for the periodic resource poll.
    visited: usize,
}

impl Renderer {
    fn render(&mut self, vm: &mut Vm<'_>, value: &JsValue, depth: usize) -> RunResult<()> {
        if depth > MAX_TREE_DEPTH {
            return Err(vm.range_error("Maximum call stack size exceeded"));
        }
        self.visited += 1;
        vm.tracker().check_memory_time_every(self.visited)?;
        match value {
            JsValue::Undefined | JsValue::Null | JsValue::Hole | JsValue::Bool(_) => Ok(()),
            JsValue::Number(number) => self.push_escaped(vm, &JsString::from_str(&number_to_string(*number))),
            JsValue::Atom(_) | JsValue::Str(_) => {
                let text = vm.string_of(value).expect("string");
                self.push_escaped(vm, &text)
            }
            JsValue::BigInt(_) => {
                let text = vm.coerce_string(value)?;
                self.push_escaped(vm, &text)
            }
            JsValue::Symbol(_) => Err(vm.type_error("Symbols are not valid as a JSX child")),
            JsValue::Ref(id) => {
                if vm.is_array(value) {
                    let items = vm.array_to_vec(value.clone_with_heap(&vm.session.heap))?;
                    self.render_all(vm, items, depth + 1)
                } else if is_node(vm, *id) {
                    self.element(vm, value, depth)
                } else {
                    Err(vm.type_error("Objects are not valid as a JSX child"))
                }
            }
        }
    }

    /// Renders owned `items` in order, releasing them all even on an error.
    fn render_all(&mut self, vm: &mut Vm<'_>, items: Vec<JsValue>, depth: usize) -> RunResult<()> {
        let mut items = items.into_iter();
        while let Some(item) = items.next() {
            let rendered = self.render(vm, &item, depth);
            item.drop_with(&mut vm.session.heap);
            if let Err(err) = rendered {
                for rest in items {
                    rest.drop_with(&mut vm.session.heap);
                }
                return Err(err);
            }
        }
        Ok(())
    }

    /// `<tag attrs>children</tag>`, or `<tag attrs>` for a void element.
    fn element(&mut self, vm: &mut Vm<'_>, node: &JsValue, depth: usize) -> RunResult<()> {
        let type_key = key(vm, "type");
        let element_type = vm.get(node, type_key)?;
        let tag = vm.string_of(&element_type);
        let got = type_name(vm, &element_type);
        element_type.drop_with(&mut vm.session.heap);
        let Some(tag) = tag else {
            let message = format!("Element type is invalid: expected a string or a function but got: {got}");
            return Err(vm.type_error(&message));
        };
        let tag_text = tag.to_string_lossy();
        self.push_str(vm, "<")?;
        self.push_units(vm, tag.units())?;
        let props_key = key(vm, "props");
        let props = vm.get(node, props_key)?;
        let inner_html = self.attributes(vm, &props);
        props.drop_with(&mut vm.session.heap);
        let inner_html = inner_html?;
        self.push_str(vm, ">")?;
        if VOID_ELEMENTS.contains(&tag_text.as_str()) {
            return Ok(());
        }
        if let Some(html) = inner_html {
            self.push_units(vm, html.units())?;
        } else {
            let children_key = key(vm, "children");
            let children = vm.get(node, children_key)?;
            let rendered = self.render(vm, &children, depth + 1);
            children.drop_with(&mut vm.session.heap);
            rendered?;
        }
        self.push_str(vm, "</")?;
        self.push_units(vm, tag.units())?;
        self.push_str(vm, ">")
    }

    /// Writes ` name="value"` for each prop that renders; returns
    /// `dangerouslySetInnerHTML.__html` when present.
    fn attributes(&mut self, vm: &mut Vm<'_>, props: &JsValue) -> RunResult<Option<JsString>> {
        let JsValue::Ref(props_id) = props else {
            return Ok(None);
        };
        let mut inner_html = None;
        for prop in vm.own_keys(*props_id, true) {
            let name = match prop {
                PropertyKey::Atom(atom) => vm.session.interns.atom(atom).to_string_lossy(),
                PropertyKey::Index(index) => index.to_string(),
                PropertyKey::Symbol(_) => continue,
            };
            let value = vm.get(props, prop)?;
            let written = match name.as_str() {
                "key" | "ref" | "children" => Ok(()),
                "dangerouslySetInnerHTML" => inner_html_of(vm, &value).map(|html| inner_html = html),
                _ => self.attribute(vm, &name, &value),
            };
            value.drop_with(&mut vm.session.heap);
            written?;
        }
        Ok(inner_html)
    }

    /// One attribute: React's spellings map to HTML, functions and nullish
    /// values are skipped, booleans are bare or absent, `style` objects
    /// become declarations.
    fn attribute(&mut self, vm: &mut Vm<'_>, name: &str, value: &JsValue) -> RunResult<()> {
        let name = match name {
            "className" => "class",
            "htmlFor" => "for",
            other => other,
        };
        let text = match value {
            JsValue::Undefined | JsValue::Null | JsValue::Hole => return Ok(()),
            JsValue::Bool(true) => {
                self.push_str(vm, " ")?;
                return self.push_str(vm, name);
            }
            // `aria-hidden="false"` means something; a bare `disabled={false}` does not
            JsValue::Bool(false) => {
                if name.starts_with("aria-") || name.starts_with("data-") {
                    JsString::from("false")
                } else {
                    return Ok(());
                }
            }
            JsValue::Symbol(_) => return Err(vm.type_error("Symbols are not valid as attribute values")),
            JsValue::Ref(_) if vm.is_callable(value) => return Ok(()),
            JsValue::Ref(id) if name == "style" => style_text(vm, *id)?,
            other => vm.coerce_string(other)?,
        };
        self.push_str(vm, " ")?;
        self.push_str(vm, name)?;
        self.push_str(vm, "=\"")?;
        self.push_escaped(vm, &text)?;
        self.push_str(vm, "\"")
    }

    fn push_str(&mut self, vm: &mut Vm<'_>, text: &str) -> RunResult<()> {
        self.push_units(vm, text.encode_utf16())
    }

    fn push_units(&mut self, vm: &mut Vm<'_>, units: impl Iterator<Item = u16>) -> RunResult<()> {
        vm.tracker().check_growth(self.out.len(), self.out.capacity(), 2)?;
        self.out.extend(units);
        Ok(())
    }

    /// Text or attribute content with `&`, `<`, `>`, `"` and `'` escaped.
    fn push_escaped(&mut self, vm: &mut Vm<'_>, text: &JsString) -> RunResult<()> {
        vm.tracker().check_growth(self.out.len(), self.out.capacity(), 2)?;
        for unit in text.units() {
            match unit {
                0x26 => self.out.extend("&amp;".encode_utf16()),
                0x3C => self.out.extend("&lt;".encode_utf16()),
                0x3E => self.out.extend("&gt;".encode_utf16()),
                0x22 => self.out.extend("&quot;".encode_utf16()),
                0x27 => self.out.extend("&#x27;".encode_utf16()),
                other => self.out.push(other),
            }
        }
        Ok(())
    }
}

/// The raw markup of `dangerouslySetInnerHTML={{ __html }}`, if any.
fn inner_html_of(vm: &mut Vm<'_>, value: &JsValue) -> RunResult<Option<JsString>> {
    if !matches!(value, JsValue::Ref(_)) {
        return Ok(None);
    }
    let html_key = key(vm, "__html");
    let html = vm.get(value, html_key)?;
    let text = match &html {
        JsValue::Undefined | JsValue::Null => Ok(None),
        other => vm.coerce_string(other).map(Some),
    };
    html.drop_with(&mut vm.session.heap);
    text
}

/// A `style` object as `name:value;…`: camelCase names become kebab-case,
/// custom properties (`--x`) stay, nullish and boolean values are skipped.
fn style_text(vm: &mut Vm<'_>, object: HeapId) -> RunResult<JsString> {
    let style = JsValue::Ref(object);
    let mut units: Vec<u16> = Vec::new();
    for prop in vm.own_keys(object, true) {
        let name = match prop {
            PropertyKey::Atom(atom) => vm.session.interns.atom(atom).to_string_lossy(),
            PropertyKey::Index(index) => index.to_string(),
            PropertyKey::Symbol(_) => continue,
        };
        let value = vm.get(&style, prop)?;
        let text = match &value {
            JsValue::Undefined | JsValue::Null | JsValue::Hole | JsValue::Bool(_) => None,
            JsValue::Number(number) => Some(JsString::from_str(&number_to_string(*number))),
            other => {
                let text = vm.coerce_string(other);
                match text {
                    Ok(text) if text.is_empty() => None,
                    Ok(text) => Some(text),
                    Err(err) => {
                        value.drop_with(&mut vm.session.heap);
                        return Err(err);
                    }
                }
            }
        };
        value.drop_with(&mut vm.session.heap);
        let Some(text) = text else { continue };
        if !units.is_empty() {
            units.push(u16::from(b';'));
        }
        vm.tracker().check_growth(units.len(), units.capacity(), 2)?;
        units.extend(css_property_name(&name).encode_utf16());
        units.push(u16::from(b':'));
        units.extend(text.units());
    }
    Ok(JsString::from_units(&units))
}

/// `backgroundColor` → `background-color`, `WebkitMask` → `-webkit-mask`;
/// custom properties pass through.
fn css_property_name(name: &str) -> String {
    if name.starts_with("--") {
        return name.to_owned();
    }
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
