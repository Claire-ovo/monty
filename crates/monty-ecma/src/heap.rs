//! The JavaScript instantiation of the shared heap.
//!
//! `monty_heap` owns the arena, reference counting and collector; this module
//! tells it what a JavaScript heap object is ([`HeapObject`] for
//! [`JsHeapData`]), registers the payload types ([`HeapReadOutput`],
//! [`Payload`]) and wraps the generic handles in local newtypes so the object
//! model's methods can be inherent impls.

use std::{
    mem,
    ops::{Deref, DerefMut},
};

use indexmap::IndexMap;
pub(crate) use monty_heap::{HeapId, HeapObject, Payload};
use serde::{Deserialize, Serialize};

use crate::{
    builtins::NativeFn,
    intern::{AtomId, FunctionId, SymbolId},
    string::JsString,
    value::JsValue,
};

/// The JavaScript heap.
pub(crate) type Heap = monty_heap::Heap<JsHeapData>;
/// Scoped access to the JavaScript heap; see [`monty_heap::HeapReader`].
pub(crate) type HeapReader<'a> = monty_heap::HeapReader<'a, JsHeapData>;
/// A branded pointer to a JavaScript heap entry.
pub(crate) type HeapPtr<'a> = monty_heap::HeapPtr<'a, JsHeapData>;

/// Everything the JavaScript heap stores.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum JsHeapData {
    /// A string too long or too dynamic to intern.
    #[serde(rename = "S")]
    String(JsString),
    /// Any object: ordinary, array, function, error, boxed primitive.
    #[serde(rename = "O")]
    Object(JsObject),
    /// A captured binding, shared by the closures that close over it and the
    /// frame that declared it.
    #[serde(rename = "C")]
    Cell(JsValue),
}

/// A JavaScript object: a prototype link, named properties, a dense element
/// store and the exotic behaviour its class adds.
///
/// Property order follows the specification's `OrdinaryOwnPropertyKeys`:
/// integer keys ascending, then string keys in insertion order, then symbols;
/// `own_keys` computes it from the insertion-ordered map.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JsObject {
    /// `[[Prototype]]`; an owned reference.
    #[serde(rename = "P")]
    pub proto: Option<HeapId>,
    /// Named properties in insertion order.
    #[serde(rename = "N")]
    pub props: IndexMap<PropertyKey, Slot, ahash::RandomState>,
    /// Dense indexed elements for arrays and `arguments`; `Hole` marks a gap.
    /// An array's `length` is this vector's length.
    #[serde(rename = "E")]
    pub elements: Vec<JsValue>,
    /// What kind of object this is, with the internal slots that kind needs.
    #[serde(rename = "K")]
    pub class: ObjectClass,
    /// `[[Extensible]]`: whether properties may still be added.
    #[serde(rename = "X")]
    pub extensible: bool,
}

impl JsObject {
    /// An empty ordinary object with the given prototype (an owned reference).
    pub fn ordinary(proto: Option<HeapId>) -> Self {
        Self::with_class(proto, ObjectClass::Ordinary)
    }

    /// An empty object of `class` with the given prototype (an owned reference).
    pub fn with_class(proto: Option<HeapId>, class: ObjectClass) -> Self {
        Self {
            proto,
            props: IndexMap::default(),
            elements: Vec::new(),
            class,
            extensible: true,
        }
    }

    /// Whether this object is callable.
    pub fn is_callable(&self) -> bool {
        matches!(self.class, ObjectClass::Function(_))
    }

    /// Whether any own property value, element or internal slot is a heap reference.
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        if let Some(proto) = self.proto {
            on_child(proto);
        }
        for slot in self.props.values() {
            slot.for_each_child(&mut on_child);
        }
        for value in &self.elements {
            if let Some(id) = value.heap_id() {
                on_child(id);
            }
        }
        self.class.for_each_child(on_child);
    }

    /// Pushes every owned reference for release.
    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        stack.extend(self.proto.take());
        for (_, slot) in self.props.drain(..) {
            slot.dec_ref_ids(stack);
        }
        stack.extend(self.elements.drain(..).filter_map(|value| value.heap_id()));
        self.class.dec_ref_ids(stack);
    }
}

/// A property name: an interned string, a canonical array index, or a symbol.
///
/// Array-index strings canonicalise to [`Index`](Self::Index) so `obj["1"]` and
/// `obj[1]` meet, as the specification requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum PropertyKey {
    #[serde(rename = "A")]
    Atom(AtomId),
    #[serde(rename = "I")]
    Index(u32),
    #[serde(rename = "S")]
    Symbol(SymbolId),
}

/// Property attribute bits: `[[Writable]]`, `[[Enumerable]]`, `[[Configurable]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PropFlags(u8);

impl PropFlags {
    pub const WRITABLE: Self = Self(1);
    pub const ENUMERABLE: Self = Self(2);
    pub const CONFIGURABLE: Self = Self(4);
    /// A plain data property created by assignment or a literal.
    pub const ALL: Self = Self(7);
    /// A builtin method or an intrinsic's `prototype` link: writable and
    /// configurable, hidden from enumeration.
    pub const HIDDEN: Self = Self(5);
    /// A frozen or `length`-like property.
    pub const NONE: Self = Self(0);

    /// Flags from their bit pattern (`Object.defineProperty`).
    #[inline]
    pub fn from_bits(bits: u8) -> Self {
        Self(bits & Self::ALL.0)
    }

    #[inline]
    pub fn writable(self) -> bool {
        self.0 & Self::WRITABLE.0 != 0
    }

    #[inline]
    pub fn enumerable(self) -> bool {
        self.0 & Self::ENUMERABLE.0 != 0
    }

    #[inline]
    pub fn configurable(self) -> bool {
        self.0 & Self::CONFIGURABLE.0 != 0
    }

    /// The flags without `[[Writable]]`.
    #[inline]
    pub fn read_only(self) -> Self {
        Self(self.0 & !Self::WRITABLE.0)
    }

    /// The flags without `[[Configurable]]`.
    #[inline]
    pub fn sealed(self) -> Self {
        Self(self.0 & !Self::CONFIGURABLE.0)
    }
}

/// One own property: a data value or an accessor pair.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Slot {
    #[serde(rename = "D")]
    Data { value: JsValue, flags: PropFlags },
    /// `getter`/`setter` are functions or `Undefined`.
    #[serde(rename = "A")]
    Accessor {
        getter: JsValue,
        setter: JsValue,
        flags: PropFlags,
    },
}

impl Slot {
    /// A data property with `ALL` attributes.
    pub fn data(value: JsValue) -> Self {
        Self::Data {
            value,
            flags: PropFlags::ALL,
        }
    }

    /// The property's attribute bits.
    pub fn flags(&self) -> PropFlags {
        match self {
            Self::Data { flags, .. } | Self::Accessor { flags, .. } => *flags,
        }
    }

    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        match self {
            Self::Data { value, .. } => {
                if let Some(id) = value.heap_id() {
                    on_child(id);
                }
            }
            Self::Accessor { getter, setter, .. } => {
                for value in [getter, setter] {
                    if let Some(id) = value.heap_id() {
                        on_child(id);
                    }
                }
            }
        }
    }

    fn dec_ref_ids(self, stack: &mut Vec<HeapId>) {
        match self {
            Self::Data { value, .. } => stack.extend(value.heap_id()),
            Self::Accessor { getter, setter, .. } => {
                stack.extend(getter.heap_id());
                stack.extend(setter.heap_id());
            }
        }
    }
}

/// The kind of an object and its internal slots.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ObjectClass {
    Ordinary,
    /// `length` is `elements.len()`.
    Array,
    Function(FunctionObject),
    /// An `Error` instance: carries the stack captured at construction.
    Error,
    /// The `arguments` object of a call.
    Arguments,
    /// `new Boolean(x)`, `new Number(x)`, `new String(x)`.
    BoxedBool(bool),
    BoxedNumber(f64),
    BoxedString(JsString),
    /// `Symbol.prototype` and `Object(symbol)`.
    BoxedSymbol(SymbolId),
    /// An iterator over an array's elements (`array` is an owned reference)
    /// or over the snapshot of keys a `for…in` loop walks.
    ArrayIterator {
        array: HeapId,
        index: u32,
        kind: IterKind,
    },
    /// An iterator over a string's code points.
    StringIterator {
        string: JsString,
        index: u32,
    },
}

/// What an array iterator yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum IterKind {
    Keys,
    Values,
    Entries,
}

impl ObjectClass {
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        match self {
            Self::Function(function) => function.for_each_child(on_child),
            Self::ArrayIterator { array, .. } => on_child(*array),
            _ => {}
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        match self {
            Self::Function(function) => function.dec_ref_ids(stack),
            Self::ArrayIterator { array, .. } => stack.push(*array),
            _ => {}
        }
    }
}

/// What makes a function object callable.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum FunctionObject {
    /// A compiled function with its captured bindings (owned cell references,
    /// in the order of `Function::free_slots`).
    Closure {
        func: FunctionId,
        cells: Box<[HeapId]>,
        /// The object a method was defined on, for `super` property access.
        home_object: Option<HeapId>,
        /// A class constructor's instance field initializer (an owned
        /// reference), run on every new instance.
        fields: Option<HeapId>,
    },
    /// A builtin implemented in Rust.
    Native(NativeFn),
    /// `Function.prototype.bind`'s result: `target` and `this` are owned references.
    Bound {
        target: HeapId,
        this: JsValue,
        args: Box<[JsValue]>,
    },
}

impl FunctionObject {
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        match self {
            Self::Closure {
                cells,
                home_object,
                fields,
                ..
            } => {
                for cell in cells {
                    on_child(*cell);
                }
                if let Some(home) = home_object {
                    on_child(*home);
                }
                if let Some(fields) = fields {
                    on_child(*fields);
                }
            }
            Self::Native(_) => {}
            Self::Bound { target, this, args } => {
                on_child(*target);
                if let Some(id) = this.heap_id() {
                    on_child(id);
                }
                for arg in args {
                    if let Some(id) = arg.heap_id() {
                        on_child(id);
                    }
                }
            }
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        match self {
            Self::Closure {
                cells,
                home_object,
                fields,
                ..
            } => {
                stack.extend(cells.iter().copied());
                stack.extend(home_object.take());
                stack.extend(fields.take());
            }
            Self::Native(_) => {}
            Self::Bound { target, this, args } => {
                stack.push(*target);
                stack.extend(mem::replace(this, JsValue::Undefined).heap_id());
                stack.extend(args.iter().filter_map(JsValue::heap_id));
            }
        }
    }
}

/// The JavaScript heap's per-heap state: nothing yet. Singletons live in the
/// realm, which the session owns.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct JsHeapState {}

impl HeapObject for JsHeapData {
    type Extra = JsHeapState;
    type Ref = JsValue;
    type Read<'a>
        = HeapReadOutput<'a>
    where
        Self: 'a;

    fn make_ref(id: HeapId) -> JsValue {
        JsValue::Ref(id)
    }

    fn read<'a>(ptr: HeapPtr<'a>, id: HeapId, reader: &HeapReader<'a>) -> HeapReadOutput<'a> {
        match ptr.data(reader) {
            Self::String(value) => HeapReadOutput::String(ptr.project(id, reader, value).into()),
            Self::Object(value) => HeapReadOutput::Object(ptr.project(id, reader, value).into()),
            Self::Cell(value) => HeapReadOutput::Cell(ptr.project(id, reader, value).into()),
        }
    }

    fn is_gc_tracked(&self) -> bool {
        // strings reference nothing, so they can never close a cycle
        !matches!(self, Self::String(_))
    }

    fn for_each_child_id(&self, mut on_child: impl FnMut(HeapId)) {
        match self {
            Self::String(_) => {}
            Self::Object(object) => object.for_each_child(on_child),
            Self::Cell(value) => {
                if let Some(id) = value.heap_id() {
                    on_child(id);
                }
            }
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        match self {
            Self::String(_) => {}
            Self::Object(object) => object.dec_ref_ids(stack),
            Self::Cell(value) => stack.extend(mem::replace(value, JsValue::Undefined).heap_id()),
        }
    }

    fn on_free(&mut self, _: &mut JsHeapState, _: HeapId) {}

    fn on_restore(&mut self, _: &mut JsHeapState, _: HeapId) {}
}

/// A typed read handle for any payload stored in the heap.
pub(crate) enum HeapReadOutput<'a> {
    String(HeapObjectRead<'a, JsString>),
    Object(HeapObjectRead<'a, JsObject>),
    Cell(HeapObjectRead<'a, JsValue>),
}

/// Registers one payload type: its `JsHeapData` variant and typed handle.
macro_rules! payload {
    ($variant:ident, $payload:ty) => {
        impl Payload<JsHeapData> for $payload {
            type Handle<'a>
                = HeapObjectRead<'a, $payload>
            where
                Self: 'a;

            #[inline]
            fn into_data(self) -> JsHeapData {
                JsHeapData::$variant(self)
            }

            #[inline]
            fn try_from_read<'a>(read: HeapReadOutput<'a>) -> Result<HeapObjectRead<'a, Self>, HeapReadOutput<'a>>
            where
                Self: 'a,
                JsHeapData: 'a,
            {
                if let HeapReadOutput::$variant(value) = read {
                    Ok(value)
                } else {
                    Err(read)
                }
            }
        }
    };
}

payload!(String, JsString);
payload!(Object, JsObject);
payload!(Cell, JsValue);

/// A typed, identity-free view protected by a [`HeapReader`]: `monty_heap::HeapRead`
/// carrying the object model's methods (`impl<'h> HeapRead<'h, JsObject>` in `object.rs`).
pub(crate) struct HeapRead<'a, T: ?Sized>(monty_heap::HeapRead<'a, T>);

impl<'a, T: ?Sized> Deref for HeapRead<'a, T> {
    type Target = monty_heap::HeapRead<'a, T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: ?Sized> DerefMut for HeapRead<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// A typed read handle for a complete object stored in a specific heap entry.
pub(crate) struct HeapObjectRead<'a, T: ?Sized> {
    read: HeapRead<'a, T>,
}

impl<'a, T: ?Sized> From<monty_heap::HeapObjectRead<'a, T>> for HeapObjectRead<'a, T> {
    fn from(read: monty_heap::HeapObjectRead<'a, T>) -> Self {
        let (_, read) = read.into_parts();
        Self { read: HeapRead(read) }
    }
}

impl<'a, T: ?Sized> Deref for HeapObjectRead<'a, T> {
    type Target = HeapRead<'a, T>;

    fn deref(&self) -> &Self::Target {
        &self.read
    }
}

impl<T: ?Sized> DerefMut for HeapObjectRead<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.read
    }
}
