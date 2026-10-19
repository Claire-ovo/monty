//! Sandbox values ⇄ the host's `MontyObject` graph.
//!
//! Export walks a value into one `MontyGraph` per message, sharing a node for
//! an object reached twice and cutting cycles with a `Cycle` placeholder;
//! import builds fresh sandbox objects from a graph. The mapping is in
//! `docs/limitations/javascript.md`: arrays and plain objects become `List`
//! and `Dict`, integral numbers become `Int`, functions and other exotic
//! objects become `Repr`.

use ahash::AHashMap;
use monty_types::{
    MontyDateTime, MontyObject, NamedValues,
    unstable::{MontyGraph, MontyNode, NodeId, named_values_parts, object_from_graph},
};

use crate::{
    builtins::date,
    error::RunResult,
    heap::{HeapId, ObjectClass, PropertyKey},
    intern::StaticAtom,
    repl::{error_exc_type, exc_type_to_kind},
    string::JsString,
    value::JsValue,
    vm::Vm,
};

impl Vm<'_> {
    /// Exports a value for the host.
    pub fn export(&mut self, value: &JsValue) -> RunResult<MontyObject> {
        let mut exporter = Exporter {
            graph: MontyGraph::new(),
            seen: AHashMap::new(),
            path: Vec::new(),
        };
        let root = exporter.push(self, value)?;
        object_from_graph(exporter.graph, root).map_err(|err| self.internal_error(&err.to_string()))
    }

    /// Imports a feed's named inputs as `globalThis` properties.
    pub fn import_inputs(&mut self, inputs: &NamedValues) -> RunResult<()> {
        let (graph, names) = named_values_parts(inputs);
        let mut importer = Importer::new(graph);
        let global = self.session.realm.global;
        for (name, id) in names {
            let value = importer.value(self, *id)?;
            let key = PropertyKey::Atom(self.session.interns.intern_str(name));
            self.define_data(global, key, value);
        }
        Ok(())
    }
}

/// The deepest value the exporter follows.
const MAX_EXPORT_DEPTH: usize = 1000;

/// Builds the outgoing graph.
struct Exporter {
    graph: MontyGraph,
    /// Objects already exported, for sharing.
    seen: AHashMap<HeapId, NodeId>,
    /// Objects being exported, for cycle detection.
    path: Vec<HeapId>,
}

impl Exporter {
    fn push(&mut self, vm: &mut Vm<'_>, value: &JsValue) -> RunResult<NodeId> {
        let node = match value {
            JsValue::Undefined | JsValue::Hole | JsValue::Null => MontyNode::None,
            JsValue::Bool(b) => MontyNode::Bool(*b),
            JsValue::Number(n) => number_node(*n),
            JsValue::Atom(_) | JsValue::Str(_) => {
                MontyNode::String(vm.string_of(value).expect("string").to_string_lossy())
            }
            JsValue::Symbol(symbol) => MontyNode::Repr(vm.key_display(PropertyKey::Symbol(*symbol))),
            JsValue::BigInt(_) => MontyNode::Repr("[BigInt]".to_owned()),
            JsValue::Ref(id) => return self.push_object(vm, *id),
        };
        Ok(self.graph.push(node))
    }

    fn push_object(&mut self, vm: &mut Vm<'_>, id: HeapId) -> RunResult<NodeId> {
        if let Some(node) = self.seen.get(&id) {
            return Ok(*node);
        }
        // the export recurses per nesting level on the Rust stack
        if self.path.len() >= MAX_EXPORT_DEPTH {
            return Err(vm.range_error("value is nested too deeply to send to the host"));
        }
        if self.path.contains(&id) {
            let placeholder = if vm.is_array(&JsValue::Ref(id)) {
                "[...]"
            } else {
                "{...}"
            };
            return Ok(self.graph.push(MontyNode::Cycle(placeholder.to_owned())));
        }
        let class_kind = vm.object(id).map(|o| match &o.class {
            ObjectClass::Array | ObjectClass::Arguments => Kind::Array,
            ObjectClass::Ordinary => Kind::Object,
            ObjectClass::Error => Kind::Error,
            ObjectClass::BoxedBool(b) => Kind::Primitive(JsValue::Bool(*b)),
            ObjectClass::BoxedNumber(n) => Kind::Primitive(JsValue::Number(*n)),
            ObjectClass::BoxedString(s) => Kind::BoxedString(s.clone()),
            ObjectClass::Map(_) => Kind::Map,
            ObjectClass::Set(_) => Kind::Set,
            ObjectClass::Date(t) => Kind::Date(*t),
            _ => Kind::Other,
        });
        let node = match class_kind {
            Some(Kind::Array) => {
                self.path.push(id);
                let length = vm.object(id).map_or(0, |o| o.elements.len());
                let mut items = Vec::with_capacity(length);
                for index in 0..length {
                    let element = vm
                        .object(id)
                        .and_then(|o| o.elements.get(index))
                        .map_or(JsValue::Undefined, |v| v.clone_with_heap(&vm.session.heap));
                    let pushed = self.push(vm, &element);
                    element.drop_with(&mut vm.session.heap);
                    items.push(pushed?);
                }
                self.path.pop();
                MontyNode::List(items)
            }
            Some(Kind::Object) => {
                self.path.push(id);
                let keys = vm.own_keys(id, true);
                let mut pairs = Vec::with_capacity(keys.len());
                for key in keys {
                    let name = vm.key_display(key);
                    let value = vm.get(&JsValue::Ref(id), key)?;
                    let pushed = self.push(vm, &value);
                    value.drop_with(&mut vm.session.heap);
                    let key_node = self.graph.push(MontyNode::String(name));
                    pairs.push((key_node, pushed?));
                }
                self.path.pop();
                MontyNode::Dict(pairs)
            }
            Some(Kind::Error) => {
                let message = vm.get_atom(&JsValue::Ref(id), StaticAtom::Message.id())?;
                let text = vm.string_of(&message).map(|s| s.to_string_lossy());
                message.drop_with(&mut vm.session.heap);
                let exc_type = error_exc_type(vm, id);
                MontyNode::Exception { exc_type, arg: text }
            }
            Some(Kind::Map | Kind::Set) => {
                self.path.push(id);
                let is_map = class_kind == Some(Kind::Map);
                let entries: Vec<(JsValue, JsValue)> = match vm.object(id).map(|o| &o.class) {
                    Some(ObjectClass::Map(data) | ObjectClass::Set(data)) => data
                        .entries
                        .iter()
                        .flatten()
                        .map(|e| {
                            (
                                e.key.clone_with_heap(&vm.session.heap),
                                e.value.clone_with_heap(&vm.session.heap),
                            )
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let mut keys = Vec::with_capacity(entries.len());
                let mut pairs = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let key_node = self.push(vm, &key);
                    let value_node = if is_map {
                        self.push(vm, &value).map(Some)
                    } else {
                        Ok(None)
                    };
                    vm.drop_all(key, vec![value]);
                    let key_node = key_node?;
                    match value_node? {
                        Some(value_node) => pairs.push((key_node, value_node)),
                        None => keys.push(key_node),
                    }
                }
                self.path.pop();
                if is_map {
                    MontyNode::Dict(pairs)
                } else {
                    MontyNode::Set(keys)
                }
            }
            Some(Kind::Primitive(primitive)) => return self.push(vm, &primitive),
            Some(Kind::BoxedString(string)) => MontyNode::String(string.to_string_lossy()),
            Some(Kind::Date(t)) => match date::time_to_host(t) {
                Some(datetime) => MontyNode::DateTime(datetime),
                None => MontyNode::Repr(date::inspect_value(t)),
            },
            Some(Kind::Other) | None => MontyNode::Repr(vm.inspect_short(&JsValue::Ref(id))?),
        };
        let node_id = self.graph.push(node);
        self.seen.insert(id, node_id);
        Ok(node_id)
    }
}

/// How an object exports.
#[derive(PartialEq)]
enum Kind {
    Array,
    Object,
    Error,
    Map,
    Set,
    Primitive(JsValue),
    BoxedString(JsString),
    /// A `Date`, exported as a UTC datetime.
    Date(f64),
    Other,
}

/// A number as the host sees it: an `Int` when integral and representable,
/// else a `Float` (`-0` exports as `0`).
fn number_node(n: f64) -> MontyNode {
    #[expect(clippy::cast_possible_truncation, reason = "the range check keeps the cast exact")]
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        MontyNode::Int(n as i64)
    } else {
        MontyNode::Float(n)
    }
}

/// Builds sandbox values from an incoming graph.
pub(crate) struct Importer<'g> {
    graph: &'g MontyGraph,
    /// Containers already built, so a shared node imports once.
    seen: AHashMap<NodeId, HeapId>,
}

impl<'g> Importer<'g> {
    pub(crate) fn new(graph: &'g MontyGraph) -> Self {
        Self {
            graph,
            seen: AHashMap::new(),
        }
    }

    pub(crate) fn value(&mut self, vm: &mut Vm<'_>, id: NodeId) -> RunResult<JsValue> {
        if let Some(existing) = self.seen.get(&id) {
            vm.session.heap.inc_ref(*existing);
            return Ok(JsValue::Ref(*existing));
        }
        Ok(match self.graph.node(id) {
            MontyNode::None | MontyNode::Ellipsis | MontyNode::NotImplemented => JsValue::Null,
            MontyNode::Bool(b) => JsValue::Bool(*b),
            MontyNode::Int(i) => JsValue::Number(*i as f64),
            MontyNode::BigInt(big) => {
                let text = big.to_string();
                JsValue::Number(text.parse().unwrap_or(f64::NAN))
            }
            MontyNode::Float(f) => JsValue::Number(*f),
            MontyNode::String(s) | MontyNode::Path(s) => vm.alloc_string(JsString::from(s.as_str()))?,
            MontyNode::Bytes(bytes) => {
                let items = bytes.iter().map(|b| JsValue::Number(f64::from(*b))).collect();
                let array = vm.new_array(items)?;
                self.remember(id, array)
            }
            MontyNode::List(items)
            | MontyNode::Tuple(items)
            | MontyNode::Set(items)
            | MontyNode::FrozenSet(items)
            | MontyNode::NamedTuple { values: items, .. } => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.value(vm, *item)?);
                }
                let array = vm.new_array(values)?;
                self.remember(id, array)
            }
            MontyNode::Dict(pairs) => {
                let object = vm.new_object();
                let JsValue::Ref(object_id) = &object else {
                    unreachable!()
                };
                let object_id = *object_id;
                self.seen.insert(id, object_id);
                for (key, value) in pairs {
                    let key = self.value(vm, *key)?;
                    let value = self.value(vm, *value)?;
                    let key = vm.coerce_key(key)?;
                    vm.define_data(object_id, key, value);
                }
                object
            }
            MontyNode::Exception { exc_type, arg } => {
                let kind = exc_type_to_kind(*exc_type);
                vm.new_error(kind, arg.as_deref().unwrap_or_default())
            }
            MontyNode::DateTime(datetime) => date::new_date(vm, date::time_from_host(datetime)),
            MontyNode::Date(day) => {
                let midnight = MontyDateTime {
                    year: day.year,
                    month: day.month,
                    day: day.day,
                    hour: 0,
                    minute: 0,
                    second: 0,
                    microsecond: 0,
                    offset_seconds: Some(0),
                    timezone_name: None,
                };
                date::new_date(vm, date::time_from_host(&midnight))
            }
            MontyNode::Cycle(_) => JsValue::Null,
            other => {
                let repr = match other {
                    MontyNode::Repr(text) => text.clone(),
                    MontyNode::Function { name, .. } => format!("<function {name}>"),
                    _ => format!("<{}>", self.graph.type_name(id)),
                };
                vm.alloc_string(JsString::from(repr.as_str()))?
            }
        })
    }

    /// Records a built container for sharing and returns it.
    fn remember(&mut self, id: NodeId, value: JsValue) -> JsValue {
        if let JsValue::Ref(heap_id) = &value {
            self.seen.insert(id, *heap_id);
        }
        value
    }
}
