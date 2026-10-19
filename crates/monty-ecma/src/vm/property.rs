//! The object model: property get/set/define/delete along the prototype
//! chain, arrays' dense elements, iteration and the primitives' wrappers.

use std::mem;

use super::{
    Vm,
    number::{index_u32, to_usize},
    ops::Hint,
};
use crate::{
    error::{RunResult, Throw},
    heap::{HeapId, IterKind, JsHeapData, JsObject, ObjectClass, PropFlags, PropertyKey, Slot},
    intern::{AtomId, StaticAtom, WellKnownSymbol},
    string::JsString,
    value::JsValue,
};

/// What advancing a builtin iterator produced.
pub(crate) enum IteratorStep {
    /// The object is not a builtin iterator; use the `next` protocol.
    NotBuiltin,
    Done,
    Value(JsValue),
}

/// Array elements past this many holes from the end are refused rather than
/// materialized, so `a[1e9] = 1` cannot turn into a gigabyte of holes.
const MAX_HOLE_RUN: usize = 1 << 20;

impl Vm<'_> {
    // === Reading ===

    /// `[[Get]]` on any value: objects walk their prototype chain, primitives
    /// read their wrapper prototype (and a string its `length` and indices).
    pub fn get(&mut self, receiver: &JsValue, key: PropertyKey) -> RunResult<JsValue> {
        match receiver {
            JsValue::Ref(id) => {
                let id = *id;
                self.get_from(id, key, receiver)
            }
            JsValue::Atom(_) | JsValue::Str(_) => {
                let string = self.string_of(receiver).expect("string");
                match key {
                    PropertyKey::Atom(atom) if atom == StaticAtom::Length.id() => {
                        return Ok(JsValue::Number(string.len() as f64));
                    }
                    PropertyKey::Index(index) => {
                        if let Some(unit) = string.unit_at(index as usize) {
                            return self.alloc_string(JsString::from_units(&[unit]));
                        }
                        return Ok(JsValue::Undefined);
                    }
                    _ => {}
                }
                let proto = self.session.realm.string_prototype;
                self.get_from(proto, key, receiver)
            }
            JsValue::Number(_) => {
                let proto = self.session.realm.number_prototype;
                self.get_from(proto, key, receiver)
            }
            JsValue::Bool(_) => {
                let proto = self.session.realm.boolean_prototype;
                self.get_from(proto, key, receiver)
            }
            JsValue::Symbol(_) => {
                let proto = self.session.realm.symbol_prototype;
                self.get_from(proto, key, receiver)
            }
            JsValue::BigInt(_) => Err(self.type_error("BigInt is not supported")),
            JsValue::Undefined | JsValue::Null | JsValue::Hole => {
                let shown = self.key_display(key);
                let what = if *receiver == JsValue::Null {
                    "null"
                } else {
                    "undefined"
                };
                Err(self.type_error(&format!("Cannot read properties of {what} (reading '{shown}')")))
            }
        }
    }

    /// `[[Get]]` by name on any value.
    pub fn get_atom(&mut self, receiver: &JsValue, name: AtomId) -> RunResult<JsValue> {
        self.get(receiver, PropertyKey::Atom(name))
    }

    /// Walks the prototype chain from `start`, calling a getter with `receiver`.
    fn get_from(&mut self, start: HeapId, key: PropertyKey, receiver: &JsValue) -> RunResult<JsValue> {
        let mut current = start;
        loop {
            let Some(object) = self.object(current) else {
                return Ok(JsValue::Undefined);
            };
            if let Some(value) = self.virtual_property(object, key) {
                return Ok(value);
            }
            match object.props.get(&key) {
                Some(Slot::Data { value, .. }) => return Ok(value.clone_with_heap(&self.session.heap)),
                Some(Slot::Accessor { getter, .. }) => {
                    if getter.is_nullish() {
                        return Ok(JsValue::Undefined);
                    }
                    let getter = getter.clone_with_heap(&self.session.heap);
                    return self.call(getter, receiver.clone_with_heap(&self.session.heap), Vec::new());
                }
                None => match object.proto {
                    Some(proto) => current = proto,
                    None => return Ok(JsValue::Undefined),
                },
            }
        }
    }

    /// A property an object's class computes rather than stores: an array's
    /// elements and `length`, a boxed string's units.
    fn virtual_property(&self, object: &JsObject, key: PropertyKey) -> Option<JsValue> {
        match (&object.class, &key) {
            (ObjectClass::Array | ObjectClass::Arguments, PropertyKey::Index(index)) => {
                match object.elements.get(*index as usize) {
                    Some(JsValue::Hole) | None => None,
                    Some(value) => Some(value.clone_with_heap(&self.session.heap)),
                }
            }
            (ObjectClass::Array | ObjectClass::Arguments, PropertyKey::Atom(atom))
                if *atom == StaticAtom::Length.id() =>
            {
                Some(JsValue::Number(object.elements.len() as f64))
            }
            (ObjectClass::BoxedString(string), PropertyKey::Atom(atom)) if *atom == StaticAtom::Length.id() => {
                Some(JsValue::Number(string.len() as f64))
            }
            _ => None,
        }
    }

    /// An own or inherited data property or getter result, `None` when absent.
    pub fn get_own_or_inherited(&mut self, object: HeapId, key: PropertyKey) -> RunResult<Option<JsValue>> {
        if !self.has_property(object, key) {
            return Ok(None);
        }
        self.get(&JsValue::Ref(object), key).map(Some)
    }

    /// `[[HasProperty]]`.
    pub fn has_property(&self, object: HeapId, key: PropertyKey) -> bool {
        let mut current = object;
        loop {
            let Some(object) = self.object(current) else {
                return false;
            };
            if self.has_own(object, key) {
                return true;
            }
            match object.proto {
                Some(proto) => current = proto,
                None => return false,
            }
        }
    }

    /// Whether `object` has `key` as an own property (virtual ones included).
    pub fn has_own(&self, object: &JsObject, key: PropertyKey) -> bool {
        self.virtual_property(object, key).is_some()
            || object.props.contains_key(&key)
            || matches!((&object.class, key), (ObjectClass::Array | ObjectClass::Arguments | ObjectClass::BoxedString(_), PropertyKey::Atom(a)) if a == StaticAtom::Length.id())
    }

    pub fn has_own_id(&self, object: HeapId, key: PropertyKey) -> bool {
        self.object(object).is_some_and(|object| self.has_own(object, key))
    }

    pub fn get_prototype(&self, object: HeapId) -> Option<HeapId> {
        self.object(object).and_then(|object| object.proto)
    }

    // === Writing ===

    /// `[[Set]]` on any value with strict-mode failures, consuming `value`.
    pub fn set(&mut self, receiver: &JsValue, key: PropertyKey, value: JsValue) -> RunResult<()> {
        let JsValue::Ref(id) = receiver else {
            value.drop_with(&mut self.session.heap);
            return if receiver.is_nullish() {
                let shown = self.key_display(key);
                let what = if *receiver == JsValue::Null {
                    "null"
                } else {
                    "undefined"
                };
                Err(self.type_error(&format!("Cannot set properties of {what} (setting '{shown}')")))
            } else {
                // a primitive receiver: the assignment has no effect in a
                // property-less wrapper, but a setter on the prototype would run
                let shown = self.key_display(key);
                let what = self.type_of(receiver);
                let text = self.coerce_string(receiver)?.to_string_lossy();
                Err(self.type_error(&format!("Cannot create property '{shown}' on {what} '{text}'")))
            };
        };
        let id = *id;
        // find the property along the chain: an own data slot is written, a
        // setter anywhere is called, a read-only slot anywhere throws
        let mut current = id;
        while let Some(object) = self.object(current) {
            match object.props.get(&key) {
                Some(Slot::Data { flags, .. }) => {
                    if !flags.writable() {
                        value.drop_with(&mut self.session.heap);
                        let shown = self.key_display(key);
                        let target = self.inspect_short(receiver)?;
                        return Err(self.type_error(&format!(
                            "Cannot assign to read only property '{shown}' of object '{target}'"
                        )));
                    }
                    break;
                }
                Some(Slot::Accessor { setter, .. }) => {
                    if setter.is_nullish() {
                        value.drop_with(&mut self.session.heap);
                        let shown = self.key_display(key);
                        return Err(self.type_error(&format!(
                            "Cannot set property {shown} of #<Object> which has only a getter"
                        )));
                    }
                    let setter = setter.clone_with_heap(&self.session.heap);
                    let result = self.call(setter, receiver.clone_with_heap(&self.session.heap), vec![value])?;
                    result.drop_with(&mut self.session.heap);
                    return Ok(());
                }
                None => match object.proto {
                    Some(proto) => current = proto,
                    None => break,
                },
            }
        }
        self.set_own(id, key, value)
    }

    /// Writes an own property, creating it on an extensible object.
    fn set_own(&mut self, id: HeapId, key: PropertyKey, value: JsValue) -> RunResult<()> {
        let Some(object) = self.object(id) else {
            value.drop_with(&mut self.session.heap);
            return Err(self.internal_error("set on a non-object"));
        };
        let is_array = matches!(object.class, ObjectClass::Array | ObjectClass::Arguments);
        if is_array {
            match key {
                PropertyKey::Index(index) => return self.set_element(id, index as usize, value),
                PropertyKey::Atom(atom) if atom == StaticAtom::Length.id() => {
                    let length = self.coerce_number_owned(value)?;
                    return self.set_array_length(id, length);
                }
                _ => {}
            }
        }
        let (len, capacity, extensible) = self
            .object(id)
            .map_or((0, 0, false), |o| (o.props.len(), o.props.capacity(), o.extensible));
        let object = self.object_mut(id).expect("checked above");
        if let Some(Slot::Data { value: slot, .. }) = object.props.get_mut(&key) {
            let old = mem::replace(slot, value);
            old.drop_with(&mut self.session.heap);
            return Ok(());
        }
        if !extensible {
            value.drop_with(&mut self.session.heap);
            let shown = self.key_display(key);
            return Err(self.type_error(&format!("Cannot add property {shown}, object is not extensible")));
        }
        self.session.heap.tracker.check_growth(len, capacity, 48)?;
        let object = self.object_mut(id).expect("checked above");
        object.props.insert(key, Slot::data(value));
        Ok(())
    }

    /// Writes an array element, growing the array (with holes) as needed.
    fn set_element(&mut self, array: HeapId, index: usize, value: JsValue) -> RunResult<()> {
        let len = self.object(array).map_or(0, |o| o.elements.len());
        if index >= len {
            if index - len > MAX_HOLE_RUN {
                value.drop_with(&mut self.session.heap);
                return Err(self.range_error("Array index too sparse"));
            }
            self.tracker().check_allocation((index + 1 - len) * 16)?;
            let object = self.object_mut(array).expect("array");
            object.elements.resize_with(index + 1, || JsValue::Hole);
        }
        let object = self.object_mut(array).expect("array");
        let old = mem::replace(&mut object.elements[index], value);
        old.drop_with(&mut self.session.heap);
        Ok(())
    }

    /// `array.length = n`: truncates, or extends with holes.
    pub fn set_array_length(&mut self, array: HeapId, length: f64) -> RunResult<()> {
        if length < 0.0 || length.fract() != 0.0 || length > f64::from(u32::MAX - 1) {
            return Err(self.range_error("Invalid array length"));
        }
        let length = to_usize(length);
        let current = self.object(array).map_or(0, |o| o.elements.len());
        if length < current {
            let object = self.object_mut(array).expect("array");
            let removed: Vec<JsValue> = object.elements.drain(length..).collect();
            for value in removed {
                value.drop_with(&mut self.session.heap);
            }
        } else if length > current {
            if length - current > MAX_HOLE_RUN {
                return Err(self.range_error("Array length too large"));
            }
            self.tracker().check_allocation((length - current) * 16)?;
            let object = self.object_mut(array).expect("array");
            object.elements.resize_with(length, || JsValue::Hole);
        }
        Ok(())
    }

    /// Appends to an array.
    pub fn array_push(&mut self, array: HeapId, value: JsValue) -> RunResult<()> {
        let Some(object) = self.object(array) else {
            value.drop_with(&mut self.session.heap);
            return Err(self.internal_error("push on a non-array"));
        };
        self.tracker()
            .check_growth(object.elements.len(), object.elements.capacity(), 16)?;
        self.object_mut(array).expect("array").elements.push(value);
        Ok(())
    }

    /// Defines (or overwrites) an own data property with default attributes.
    pub fn define_data(&mut self, object: HeapId, key: PropertyKey, value: JsValue) {
        self.define_slot(object, key, Slot::data(value));
    }

    /// Defines an own data property with the given attributes.
    pub fn define_with_flags(&mut self, object: HeapId, key: PropertyKey, value: JsValue, flags: PropFlags) {
        self.define_slot(object, key, Slot::Data { value, flags });
    }

    /// Defines an own property, replacing any existing one.
    pub fn define_slot(&mut self, object: HeapId, key: PropertyKey, slot: Slot) {
        let Some(target) = self.object_mut(object) else {
            return;
        };
        let slot = match (&target.class, key, slot) {
            (ObjectClass::Array | ObjectClass::Arguments, PropertyKey::Index(index), Slot::Data { value, .. }) => {
                let index = index as usize;
                // a sparse index past the hole limit is dropped
                let _ = self.set_element(object, index, value);
                return;
            }
            (_, _, slot) => slot,
        };
        let old = target.props.insert(key, slot);
        if let Some(old) = old {
            let mut ids = Vec::new();
            match old {
                Slot::Data { value, .. } => ids.extend(value.heap_id()),
                Slot::Accessor { getter, setter, .. } => {
                    ids.extend(getter.heap_id());
                    ids.extend(setter.heap_id());
                }
            }
            for id in ids {
                self.session.heap.dec_ref(id);
            }
        }
    }

    /// Defines a getter or setter, merging with an existing accessor pair.
    pub fn define_accessor(
        &mut self,
        object: HeapId,
        key: PropertyKey,
        function: JsValue,
        is_setter: bool,
        enumerable: bool,
    ) {
        let existing = self
            .object_mut(object)
            .and_then(|target| target.props.shift_remove(&key));
        let (mut getter, mut setter) = match existing {
            Some(Slot::Accessor { getter, setter, .. }) => (getter, setter),
            Some(Slot::Data { value, .. }) => {
                value.drop_with(&mut self.session.heap);
                (JsValue::Undefined, JsValue::Undefined)
            }
            None => (JsValue::Undefined, JsValue::Undefined),
        };
        let replaced = if is_setter {
            mem::replace(&mut setter, function)
        } else {
            mem::replace(&mut getter, function)
        };
        replaced.drop_with(&mut self.session.heap);
        let flags = if enumerable {
            PropFlags::ALL.read_only()
        } else {
            PropFlags::HIDDEN.read_only()
        };
        if let Some(target) = self.object_mut(object) {
            target.props.insert(key, Slot::Accessor { getter, setter, flags });
        }
    }

    /// `[[Delete]]` with strict-mode failure on a non-configurable property.
    pub fn delete(&mut self, receiver: &JsValue, key: PropertyKey) -> RunResult<bool> {
        let JsValue::Ref(id) = receiver else {
            if receiver.is_nullish() {
                let shown = self.key_display(key);
                let what = if *receiver == JsValue::Null {
                    "null"
                } else {
                    "undefined"
                };
                return Err(self.type_error(&format!("Cannot convert {what} to object (deleting '{shown}')")));
            }
            return Ok(true);
        };
        let id = *id;
        let Some(object) = self.object(id) else {
            return Ok(true);
        };
        if let (ObjectClass::Array | ObjectClass::Arguments, PropertyKey::Index(index)) = (&object.class, key) {
            let index = index as usize;
            let object = self.object_mut(id).expect("array");
            if index < object.elements.len() {
                let old = mem::replace(&mut object.elements[index], JsValue::Hole);
                old.drop_with(&mut self.session.heap);
            }
            return Ok(true);
        }
        match object.props.get(&key) {
            None => Ok(true),
            Some(slot) if !slot.flags().configurable() => {
                let shown = self.key_display(key);
                Err(self.type_error(&format!("Cannot delete property '{shown}' of #<Object>")))
            }
            Some(_) => {
                let object = self.object_mut(id).expect("object");
                let removed = object.props.shift_remove(&key).expect("present");
                let mut ids = Vec::new();
                match removed {
                    Slot::Data { value, .. } => ids.extend(value.heap_id()),
                    Slot::Accessor { getter, setter, .. } => {
                        ids.extend(getter.heap_id());
                        ids.extend(setter.heap_id());
                    }
                }
                for id in ids {
                    self.session.heap.dec_ref(id);
                }
                Ok(true)
            }
        }
    }

    /// `{ ...source }`: copies own enumerable properties.
    pub fn copy_data_properties(&mut self, target: HeapId, source: &JsValue) -> RunResult<()> {
        let keys = match source {
            JsValue::Ref(id) => self.own_keys(*id, true),
            JsValue::Atom(_) | JsValue::Str(_) => {
                let string = self.string_of(source).expect("string");
                for (index, unit) in string.units().enumerate() {
                    let value = self.alloc_string(JsString::from_units(&[unit]))?;
                    self.define_data(target, PropertyKey::Index(index_u32(index)), value);
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        for key in keys {
            let value = self.get(source, key)?;
            self.define_data(target, key, value);
        }
        Ok(())
    }

    /// Sets `[[Prototype]]`, refusing a cycle.
    pub fn set_prototype(&mut self, object: HeapId, proto: Option<HeapId>) {
        let mut current = proto;
        while let Some(id) = current {
            if id == object {
                return;
            }
            current = self.get_prototype(id);
        }
        if let Some(proto) = proto {
            self.session.heap.inc_ref(proto);
        }
        if let Some(target) = self.object_mut(object) {
            let old = mem::replace(&mut target.proto, proto);
            if let Some(old) = old {
                self.session.heap.dec_ref(old);
            }
        } else if let Some(proto) = proto {
            self.session.heap.dec_ref(proto);
        }
    }

    // === Keys ===

    /// Own property keys in specification order: array indices ascending,
    /// then strings in insertion order, then symbols. `enumerable_only`
    /// filters to enumerable string keys (`for…in`, `Object.keys`, spread).
    pub fn own_keys(&self, object: HeapId, enumerable_only: bool) -> Vec<PropertyKey> {
        let Some(object) = self.object(object) else {
            return Vec::new();
        };
        let mut indices: Vec<u32> = Vec::new();
        let mut strings = Vec::new();
        let mut symbols = Vec::new();
        match &object.class {
            ObjectClass::Array | ObjectClass::Arguments => {
                indices.extend(
                    object
                        .elements
                        .iter()
                        .enumerate()
                        .filter(|(_, v)| **v != JsValue::Hole)
                        .map(|(i, _)| index_u32(i)),
                );
            }
            ObjectClass::BoxedString(string) => indices.extend(0..index_u32(string.len())),
            _ => {}
        }
        for (key, slot) in &object.props {
            if enumerable_only && !slot.flags().enumerable() {
                continue;
            }
            match key {
                PropertyKey::Index(index) => indices.push(*index),
                PropertyKey::Atom(_) => strings.push(*key),
                PropertyKey::Symbol(_) => symbols.push(*key),
            }
        }
        indices.sort_unstable();
        let mut keys: Vec<PropertyKey> = indices.into_iter().map(PropertyKey::Index).collect();
        keys.extend(strings);
        if !enumerable_only {
            keys.extend(symbols);
        }
        keys
    }

    /// `ToPropertyKey`, consuming the value.
    pub fn coerce_key(&mut self, value: JsValue) -> RunResult<PropertyKey> {
        match value {
            JsValue::Symbol(symbol) => Ok(PropertyKey::Symbol(symbol)),
            JsValue::Number(n) if n >= 0.0 && n.fract() == 0.0 && n < f64::from(u32::MAX) => {
                Ok(PropertyKey::Index(index_u32(to_usize(n))))
            }
            JsValue::Atom(atom) => {
                let string = self.session.interns.atom(atom);
                Ok(string
                    .as_array_index()
                    .map_or(PropertyKey::Atom(atom), PropertyKey::Index))
            }
            JsValue::Ref(_) => {
                let primitive = self.coerce_primitive(&value, Hint::String);
                value.drop_with(&mut self.session.heap);
                self.coerce_key(primitive?)
            }
            other => {
                let string = self.coerce_string(&other);
                other.drop_with(&mut self.session.heap);
                let string = string?;
                Ok(match string.as_array_index() {
                    Some(index) => PropertyKey::Index(index),
                    None => PropertyKey::Atom(self.session.interns.intern(string, &self.session.heap.tracker)?),
                })
            }
        }
    }

    /// `ToPropertyKey` for `object[key]`, raising the access error first
    /// when the object is nullish (so the message names the key).
    pub fn coerce_key_for(&mut self, object: &JsValue, key: JsValue) -> RunResult<PropertyKey> {
        if object.is_nullish() {
            let shown = match &key {
                JsValue::Ref(_) => "[object Object]".to_owned(),
                other => self.coerce_string(other)?.to_string_lossy(),
            };
            key.drop_with(&mut self.session.heap);
            let what = if *object == JsValue::Null { "null" } else { "undefined" };
            return Err(self.type_error(&format!("Cannot read properties of {what} (reading '{shown}')")));
        }
        self.coerce_key(key)
    }

    /// The value form of a key (a string or symbol).
    pub fn key_to_value(&mut self, key: PropertyKey) -> JsValue {
        match key {
            PropertyKey::Atom(atom) => JsValue::Atom(atom),
            PropertyKey::Index(index) => self.atom_value(&index.to_string()),
            PropertyKey::Symbol(symbol) => JsValue::Symbol(symbol),
        }
    }

    /// A key as error messages show it.
    pub fn key_display(&self, key: PropertyKey) -> String {
        match key {
            PropertyKey::Atom(atom) => self.session.interns.atom(atom).to_string_lossy(),
            PropertyKey::Index(index) => index.to_string(),
            PropertyKey::Symbol(symbol) => {
                let description = self.session.interns.symbol(symbol).description.as_ref();
                format!(
                    "Symbol({})",
                    description.map(JsString::to_string_lossy).unwrap_or_default()
                )
            }
        }
    }

    /// A would-be key as the `in` operator's error shows it.
    pub fn display_key_value(&mut self, value: &JsValue) -> RunResult<String> {
        match value {
            JsValue::Symbol(symbol) => Ok(self.key_display(PropertyKey::Symbol(*symbol))),
            JsValue::Ref(_) => Ok("[object Object]".to_owned()),
            other => Ok(self.coerce_string(other)?.to_string_lossy()),
        }
    }

    // === Allocation ===

    /// A new empty object with `Object.prototype`.
    pub fn new_object(&mut self) -> JsValue {
        let proto = self.session.realm.object_prototype;
        self.new_object_with_proto(Some(proto))
    }

    /// A new empty ordinary object with the given prototype.
    pub fn new_object_with_proto(&mut self, proto: Option<HeapId>) -> JsValue {
        if let Some(proto) = proto {
            self.session.heap.inc_ref(proto);
        }
        JsValue::Ref(
            self.session
                .heap
                .allocate(JsHeapData::Object(JsObject::ordinary(proto))),
        )
    }

    /// A new object of `class` with the given prototype.
    pub fn new_class_object(&mut self, proto: HeapId, class: ObjectClass) -> HeapId {
        self.session.heap.inc_ref(proto);
        self.session
            .heap
            .allocate(JsHeapData::Object(JsObject::with_class(Some(proto), class)))
    }

    /// A new array holding `items`.
    pub fn new_array(&mut self, items: Vec<JsValue>) -> RunResult<JsValue> {
        self.tracker().check_allocation(items.len() * 16)?;
        let proto = self.session.realm.array_prototype;
        let id = self.new_class_object(proto, ObjectClass::Array);
        self.object_mut(id).expect("array").elements = items;
        Ok(JsValue::Ref(id))
    }

    /// Whether a value is an `Array`.
    pub fn is_array(&self, value: &JsValue) -> bool {
        matches!(value, JsValue::Ref(id) if matches!(self.object(*id).map(|o| &o.class), Some(ObjectClass::Array)))
    }

    /// The elements of an array value (holes as `undefined`), consuming it.
    pub fn array_to_vec(&mut self, array: JsValue) -> RunResult<Vec<JsValue>> {
        let JsValue::Ref(id) = &array else {
            array.drop_with(&mut self.session.heap);
            return Err(self.internal_error("spread target is not an array"));
        };
        let items: Vec<JsValue> = match self.object(*id) {
            Some(object) => object
                .elements
                .iter()
                .map(|v| match v {
                    JsValue::Hole => JsValue::Undefined,
                    other => other.clone_with_heap(&self.session.heap),
                })
                .collect(),
            None => Vec::new(),
        };
        array.drop_with(&mut self.session.heap);
        Ok(items)
    }

    // === Iteration ===

    /// Destructuring's `IteratorStep` on the iterator at the top of the stack:
    /// the next value, or `undefined` once done (the exhausted iterator is
    /// replaced by `undefined` so `IteratorClose` leaves it alone).
    pub fn iterator_step(&mut self) -> RunResult<JsValue> {
        let iterator = self.pop();
        if iterator == JsValue::Undefined {
            self.stack.push(iterator);
            return Ok(JsValue::Undefined);
        }
        let next = self.iterator_next(&iterator);
        match next {
            Ok(Some(value)) => {
                self.stack.push(iterator);
                Ok(value)
            }
            Ok(None) => {
                iterator.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Undefined);
                Ok(JsValue::Undefined)
            }
            Err(err) => {
                // a throwing iterator is not closed
                iterator.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Undefined);
                Err(err)
            }
        }
    }

    /// Destructuring's `IteratorRest`: the remaining values as an array; the
    /// iterator is exhausted afterwards.
    pub fn iterator_rest(&mut self) -> RunResult<JsValue> {
        let iterator = self.pop();
        self.stack.push(JsValue::Undefined);
        if iterator == JsValue::Undefined {
            return self.new_array(Vec::new());
        }
        let mut items: Vec<JsValue> = Vec::new();
        loop {
            match self.iterator_next(&iterator) {
                Ok(Some(value)) => {
                    if let Err(err) = self.tracker().check_growth(items.len(), items.capacity(), 16) {
                        value.drop_with(&mut self.session.heap);
                        self.drop_all(iterator, items);
                        return Err(err.into());
                    }
                    items.push(value);
                }
                Ok(None) => break,
                Err(err) => {
                    self.drop_all(iterator, items);
                    return Err(err);
                }
            }
        }
        iterator.drop_with(&mut self.session.heap);
        self.new_array(items)
    }

    /// `GetIterator` for a destructuring pattern, naming `source` (the
    /// initializer's text) in the error when the value is not iterable.
    pub fn get_iterator_named(&mut self, iterable: JsValue, source: AtomId) -> RunResult<JsValue> {
        if self.is_array(&iterable) || iterable.is_string() || self.is_iterable_object(&iterable) {
            return self.get_iterator(iterable);
        }
        let message = if source == StaticAtom::Empty.id() {
            format!(
                "{} is not iterable (cannot read property Symbol(Symbol.iterator))",
                self.describe_for_error(&iterable)?
            )
        } else {
            format!(
                "{} is not iterable",
                self.session.interns.atom(source).to_string_lossy()
            )
        };
        iterable.drop_with(&mut self.session.heap);
        Err(self.type_error(&message))
    }

    /// Whether an object value has a callable `Symbol.iterator`.
    fn is_iterable_object(&mut self, value: &JsValue) -> bool {
        let JsValue::Ref(_) = value else {
            return false;
        };
        let method = self.get(value, PropertyKey::Symbol(WellKnownSymbol::Iterator.id()));
        match method {
            Ok(method) => {
                let callable = self.is_callable(&method);
                method.drop_with(&mut self.session.heap);
                callable
            }
            Err(err) => {
                if let Throw::Value(value) = err {
                    value.drop_with(&mut self.session.heap);
                }
                false
            }
        }
    }

    /// A value as V8 names it in type errors: the type, then the value for
    /// primitives (`number 5`, `object null`, `undefined`, `object`).
    pub fn describe_for_error(&mut self, value: &JsValue) -> RunResult<String> {
        Ok(match value {
            JsValue::Undefined | JsValue::Hole => "undefined".to_owned(),
            JsValue::Null => "object null".to_owned(),
            JsValue::Bool(b) => format!("boolean {b}"),
            JsValue::Number(_) => format!("number {}", self.inspect(value)?),
            JsValue::Atom(_) | JsValue::Str(_) => format!("string \"{}\"", self.string_of(value).expect("string")),
            JsValue::Symbol(_) => "symbol".to_owned(),
            JsValue::BigInt(_) => "bigint".to_owned(),
            JsValue::Ref(_) => "object".to_owned(),
        })
    }

    /// `GetIterator`: an array or string gets a builtin iterator; anything
    /// else must supply `Symbol.iterator`. Consumes `iterable`.
    pub fn get_iterator(&mut self, iterable: JsValue) -> RunResult<JsValue> {
        if let JsValue::Ref(id) = &iterable
            && matches!(self.object(*id).map(|o| &o.class), Some(ObjectClass::Array))
            && !self.has_own_id(*id, PropertyKey::Symbol(WellKnownSymbol::Iterator.id()))
        {
            let iterator = self.array_iterator(*id, IterKind::Values);
            iterable.drop_with(&mut self.session.heap);
            return Ok(iterator);
        }
        if iterable.is_string() {
            let string = self.string_of(&iterable).expect("string");
            iterable.drop_with(&mut self.session.heap);
            let proto = self.session.realm.array_iterator_prototype;
            let id = self.new_class_object(proto, ObjectClass::StringIterator { string, index: 0 });
            return Ok(JsValue::Ref(id));
        }
        let method = self.get(&iterable, PropertyKey::Symbol(WellKnownSymbol::Iterator.id()));
        let method = match method {
            Ok(method) => method,
            Err(err) => {
                iterable.drop_with(&mut self.session.heap);
                return Err(err);
            }
        };
        if !self.is_callable(&method) {
            method.drop_with(&mut self.session.heap);
            let shown = self.inspect_short(&iterable)?;
            iterable.drop_with(&mut self.session.heap);
            return Err(self.type_error(&format!("{shown} is not iterable")));
        }
        let iterator = self.call(method, iterable, Vec::new())?;
        if !iterator.is_object() {
            iterator.drop_with(&mut self.session.heap);
            return Err(self.type_error("Result of the Symbol.iterator method is not an object"));
        }
        Ok(iterator)
    }

    /// A builtin iterator over an array (which it keeps alive).
    pub fn array_iterator(&mut self, array: HeapId, kind: IterKind) -> JsValue {
        self.session.heap.inc_ref(array);
        let proto = self.session.realm.array_iterator_prototype;
        let id = self.new_class_object(proto, ObjectClass::ArrayIterator { array, index: 0, kind });
        JsValue::Ref(id)
    }

    /// The next value of an iterator, `None` when done.
    pub fn iterator_next(&mut self, iterator: &JsValue) -> RunResult<Option<JsValue>> {
        let JsValue::Ref(id) = iterator else {
            return Err(self.type_error("iterator is not an object"));
        };
        let id = *id;
        match self.builtin_iterator_next(id)? {
            IteratorStep::Value(value) => return Ok(Some(value)),
            IteratorStep::Done => return Ok(None),
            IteratorStep::NotBuiltin => {}
        }
        let next = self.get_atom(iterator, StaticAtom::Next.id())?;
        let result = self.call(next, iterator.clone_with_heap(&self.session.heap), Vec::new())?;
        if !result.is_object() {
            let shown = self.inspect_short(&result)?;
            result.drop_with(&mut self.session.heap);
            return Err(self.type_error(&format!("Iterator result {shown} is not an object")));
        }
        let done = self.get_atom(&result, StaticAtom::Done.id())?;
        let is_done = self.to_boolean(&done);
        done.drop_with(&mut self.session.heap);
        let value = if is_done {
            None
        } else {
            Some(self.get_atom(&result, StaticAtom::Value.id())?)
        };
        result.drop_with(&mut self.session.heap);
        Ok(value)
    }

    /// Advances a builtin iterator.
    pub fn builtin_iterator_next(&mut self, id: HeapId) -> RunResult<IteratorStep> {
        let step = match self.object(id).map(|o| &o.class) {
            Some(ObjectClass::ArrayIterator { array, index, kind }) => {
                let (array, index, kind) = (*array, *index, *kind);
                let len = self.object(array).map_or(0, |o| o.elements.len());
                if index as usize >= len {
                    None
                } else {
                    let element = self.object(array).and_then(|o| o.elements.get(index as usize)).map_or(
                        JsValue::Undefined,
                        |v| match v {
                            JsValue::Hole => JsValue::Undefined,
                            other => other.clone_with_heap(&self.session.heap),
                        },
                    );
                    let value = match kind {
                        IterKind::Values => element,
                        IterKind::Keys => {
                            element.drop_with(&mut self.session.heap);
                            JsValue::Number(f64::from(index))
                        }
                        IterKind::Entries => self.new_array(vec![JsValue::Number(f64::from(index)), element])?,
                    };
                    if let Some(ObjectClass::ArrayIterator { index, .. }) = self.object_mut(id).map(|o| &mut o.class) {
                        *index += 1;
                    }
                    Some(value)
                }
            }
            Some(ObjectClass::StringIterator { string, index }) => {
                let index = *index as usize;
                match string.unit_at(index) {
                    None => None,
                    Some(unit) => {
                        let mut units = vec![unit];
                        if (0xD800..0xDC00).contains(&unit)
                            && let Some(low) = string.unit_at(index + 1)
                            && (0xDC00..0xE000).contains(&low)
                        {
                            units.push(low);
                        }
                        let advance = index_u32(units.len());
                        let value = self.alloc_string(JsString::from_units(&units))?;
                        if let Some(ObjectClass::StringIterator { index, .. }) =
                            self.object_mut(id).map(|o| &mut o.class)
                        {
                            *index += advance;
                        }
                        Some(value)
                    }
                }
            }
            _ => return Ok(IteratorStep::NotBuiltin),
        };
        Ok(step.map_or(IteratorStep::Done, IteratorStep::Value))
    }

    /// `IteratorClose`: calls `return` on a user iterator.
    pub fn iterator_close(&mut self, iterator: &JsValue) -> RunResult<()> {
        let JsValue::Ref(id) = iterator else {
            return Ok(());
        };
        if matches!(
            self.object(*id).map(|o| &o.class),
            Some(ObjectClass::ArrayIterator { .. } | ObjectClass::StringIterator { .. })
        ) {
            return Ok(());
        }
        let method = self.get_atom(iterator, StaticAtom::Return.id())?;
        if self.is_callable(&method) {
            let result = self.call(method, iterator.clone_with_heap(&self.session.heap), Vec::new())?;
            result.drop_with(&mut self.session.heap);
        } else {
            method.drop_with(&mut self.session.heap);
        }
        Ok(())
    }

    /// Collects an iterable into a vector (array spread, `Array.from`).
    pub fn iterate_to_vec(&mut self, iterable: JsValue) -> RunResult<Vec<JsValue>> {
        if self.is_array(&iterable) {
            return self.array_to_vec(iterable);
        }
        let iterator = self.get_iterator(iterable)?;
        let mut items: Vec<JsValue> = Vec::new();
        loop {
            match self.iterator_next(&iterator) {
                Ok(Some(value)) => {
                    if let Err(err) = self.tracker().check_growth(items.len(), items.capacity(), 16) {
                        value.drop_with(&mut self.session.heap);
                        iterator.drop_with(&mut self.session.heap);
                        for item in items {
                            item.drop_with(&mut self.session.heap);
                        }
                        return Err(err.into());
                    }
                    items.push(value);
                }
                Ok(None) => break,
                Err(err) => {
                    iterator.drop_with(&mut self.session.heap);
                    for item in items {
                        item.drop_with(&mut self.session.heap);
                    }
                    return Err(err);
                }
            }
        }
        iterator.drop_with(&mut self.session.heap);
        Ok(items)
    }

    /// `for…in`: an iterator over the enumerable string keys of the object
    /// and its prototypes, snapshotted up front. Consumes `object`.
    pub fn for_in_iterator(&mut self, object: JsValue) -> RunResult<JsValue> {
        let mut keys: Vec<JsValue> = Vec::new();
        let mut seen: Vec<PropertyKey> = Vec::new();
        let mut current = match &object {
            JsValue::Ref(id) => Some(*id),
            JsValue::Atom(_) | JsValue::Str(_) => {
                let string = self.string_of(&object).expect("string");
                for index in 0..string.len() {
                    keys.push(self.atom_value(&index.to_string()));
                }
                Some(self.session.realm.string_prototype)
            }
            _ => None,
        };
        while let Some(id) = current {
            for key in self.own_keys(id, false) {
                if matches!(key, PropertyKey::Symbol(_)) || seen.contains(&key) {
                    continue;
                }
                let enumerable = self
                    .object(id)
                    .and_then(|o| o.props.get(&key))
                    .is_none_or(|slot| slot.flags().enumerable());
                seen.push(key);
                if enumerable {
                    let value = self.key_to_value(key);
                    keys.push(value);
                }
            }
            current = self.get_prototype(id);
        }
        object.drop_with(&mut self.session.heap);
        let array = self.new_array(keys)?;
        let JsValue::Ref(array_id) = array else { unreachable!() };
        let iterator = self.array_iterator(array_id, IterKind::Values);
        array.drop_with(&mut self.session.heap);
        Ok(iterator)
    }
}
