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
    sync::Arc,
};

use ahash::AHashMap;
use indexmap::IndexMap;
pub(crate) use monty_heap::{HeapId, HeapObject, Payload};
use monty_regexp::{CompiledRegExp, RegExpFlags};
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
    Map(Box<MapData>),
    Set(Box<MapData>),
    /// Holds its keys strongly: there is no finalization in the sandbox.
    WeakMap(Box<MapData>),
    WeakSet(Box<MapData>),
    /// An iterator over a `Map` or `Set` (`map` is an owned reference); it
    /// walks the entry vector by index, so entries added while iterating are
    /// seen and deleted ones (tombstones) skipped.
    MapIterator {
        map: HeapId,
        index: u32,
        kind: IterKind,
    },
    RegExp(Box<RegExpData>),
    /// The iterator `matchAll` returns: `regexp` is an owned reference to a
    /// private copy of the pattern whose `lastIndex` tracks the position.
    RegExpStringIterator {
        regexp: HeapId,
        string: JsString,
        done: bool,
    },
    Generator(Box<GeneratorState>),
    Promise(Box<PromiseData>),
    /// An async function's activation (never user-visible): its parked frame
    /// and the promise (an owned reference) its return settles.
    Coroutine(Box<Coroutine>),
}

/// A promise's settlement state and whether a rejection has a handler.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PromiseData {
    pub state: PromiseState,
    /// Set once a reaction is attached to a rejected promise.
    pub handled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum PromiseState {
    Pending(Vec<Reaction>),
    Fulfilled(JsValue),
    Rejected(JsValue),
}

/// What runs when a promise settles: one handler per outcome, feeding `target`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Reaction {
    pub target: Target,
    pub on_fulfilled: Handler,
    pub on_rejected: Handler,
}

/// Where a reaction's result goes (both references are owned; the enum is
/// `Copy` for convenience, the owner releases the reference exactly once).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) enum Target {
    /// Resolve or reject this promise with the handler's outcome.
    Promise(HeapId),
    /// Resume this async function's frame with the settlement.
    Coroutine(HeapId),
    /// Discard the outcome (`await`-free bookkeeping such as `Promise.all` elements).
    None,
}

/// How a settlement is transformed before reaching the target.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Handler {
    /// Pass the value through (fulfill the target with it, or resume `next`).
    Identity,
    /// Pass the reason through (reject the target, or resume `throw`).
    Thrower,
    /// Call this function with the value; its result (or throw) decides the target.
    Call(JsValue),
    /// `finally`: call this function without arguments, then settle the
    /// target with the original settlement unless the call failed.
    Finally(JsValue),
    /// Settle the target with a stored settlement, ignoring the value.
    Settle(Settled),
}

/// A promise's outcome.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Settled {
    Fulfilled(JsValue),
    Rejected(JsValue),
}

impl Settled {
    pub fn value(&self) -> &JsValue {
        match self {
            Self::Fulfilled(value) | Self::Rejected(value) => value,
        }
    }

    pub fn into_value(self) -> JsValue {
        match self {
            Self::Fulfilled(value) | Self::Rejected(value) => value,
        }
    }
}

impl Handler {
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        match self {
            Self::Identity | Self::Thrower => {}
            Self::Call(function) | Self::Finally(function) => {
                if let Some(id) = function.heap_id() {
                    on_child(id);
                }
            }
            Self::Settle(settled) => {
                if let Some(id) = settled.value().heap_id() {
                    on_child(id);
                }
            }
        }
    }

    fn dec_ref_ids(self, stack: &mut Vec<HeapId>) {
        match self {
            Self::Identity | Self::Thrower => {}
            Self::Call(function) | Self::Finally(function) => stack.extend(function.heap_id()),
            Self::Settle(settled) => stack.extend(settled.into_value().heap_id()),
        }
    }
}

impl Reaction {
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        match &self.target {
            Target::Promise(id) | Target::Coroutine(id) => on_child(*id),
            Target::None => {}
        }
        self.on_fulfilled.for_each_child(&mut on_child);
        self.on_rejected.for_each_child(on_child);
    }

    fn dec_ref_ids(self, stack: &mut Vec<HeapId>) {
        match self.target {
            Target::Promise(id) | Target::Coroutine(id) => stack.push(id),
            Target::None => {}
        }
        self.on_fulfilled.dec_ref_ids(stack);
        self.on_rejected.dec_ref_ids(stack);
    }
}

impl PromiseData {
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        match &self.state {
            PromiseState::Pending(reactions) => {
                for reaction in reactions {
                    reaction.for_each_child(&mut on_child);
                }
            }
            PromiseState::Fulfilled(value) | PromiseState::Rejected(value) => {
                if let Some(id) = value.heap_id() {
                    on_child(id);
                }
            }
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        match mem::replace(&mut self.state, PromiseState::Pending(Vec::new())) {
            PromiseState::Pending(reactions) => {
                for reaction in reactions {
                    reaction.dec_ref_ids(stack);
                }
            }
            PromiseState::Fulfilled(value) | PromiseState::Rejected(value) => stack.extend(value.heap_id()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Coroutine {
    pub state: GeneratorState,
    pub promise: HeapId,
}

/// Where a generator is in its life: its frame is parked here between
/// `next` calls, taken out while it runs, and gone once the body returns.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum GeneratorState {
    Suspended(SavedFrame),
    Running,
    Completed,
}

/// A generator's or async function's frame between resumptions: the
/// function, the function object it ran as (an owned reference; `None` for
/// an async module body), the instruction to continue at, and its locals and
/// operands as they were on the stack.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SavedFrame {
    #[serde(rename = "F")]
    pub function: FunctionId,
    #[serde(rename = "C")]
    pub callee: Option<HeapId>,
    #[serde(rename = "I")]
    pub ip: u32,
    #[serde(rename = "V")]
    pub values: Vec<JsValue>,
}

impl GeneratorState {
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        if let Self::Suspended(frame) = self {
            if let Some(callee) = frame.callee {
                on_child(callee);
            }
            for value in &frame.values {
                if let Some(id) = value.heap_id() {
                    on_child(id);
                }
            }
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        if let Self::Suspended(frame) = mem::replace(self, Self::Completed) {
            stack.extend(frame.callee);
            stack.extend(frame.values.into_iter().filter_map(|value| value.heap_id()));
        }
    }
}

/// A `RegExp` object's internal slots: the pattern text and flags it was
/// built from. The compiled program is derived from them on first use (and
/// again after a restore), so it is never serialized.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RegExpData {
    #[serde(rename = "S")]
    pub source: JsString,
    /// `RegExpFlags` bits.
    #[serde(rename = "F")]
    pub flags: u16,
    #[serde(skip)]
    pub program: Option<Arc<CompiledRegExp>>,
}

impl RegExpData {
    /// The typed flags.
    pub fn flags(&self) -> RegExpFlags {
        RegExpFlags::from_bits(self.flags)
    }
}

/// The storage of `Map`, `Set` and the weak collections: insertion-ordered
/// entries, deleted ones left as tombstones so iterators keep their place,
/// indexed by the key's `SameValueZero` identity.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct MapData {
    #[serde(rename = "E")]
    pub entries: Vec<Option<MapEntry>>,
    /// Live entries (entries minus tombstones).
    #[serde(rename = "L")]
    pub live: usize,
    /// Rebuilt from the entries' hashes on restore.
    #[serde(skip)]
    pub index: AHashMap<MapKey, usize>,
}

impl MapData {
    fn for_each_child(&self, mut on_child: impl FnMut(HeapId)) {
        for entry in self.entries.iter().flatten() {
            if let Some(id) = entry.key.heap_id() {
                on_child(id);
            }
            if let Some(id) = entry.value.heap_id() {
                on_child(id);
            }
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        for entry in self.entries.drain(..).flatten() {
            stack.extend(entry.key.heap_id());
            stack.extend(entry.value.heap_id());
        }
        self.index.clear();
        self.live = 0;
    }

    /// Rebuilds the key index after deserialization.
    pub fn rebuild_index(&mut self) {
        self.index = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_ref().map(|entry| (entry.hash.clone(), index)))
            .collect();
    }
}

/// One `key => value` of a collection (a set stores `undefined` values).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MapEntry {
    #[serde(rename = "K")]
    pub key: JsValue,
    #[serde(rename = "V")]
    pub value: JsValue,
    /// The key's identity under `SameValueZero`.
    #[serde(rename = "H")]
    pub hash: MapKey,
}

/// A collection key's identity: `SameValueZero`, so `NaN` matches itself,
/// `-0` is `0`, strings match by content and objects by heap identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum MapKey {
    Undefined,
    Null,
    Bool(bool),
    /// The canonical bit pattern (`NaN` normalized, `-0` as `0`).
    Number(u64),
    String(JsString),
    Symbol(SymbolId),
    Ref(HeapId),
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
            Self::MapIterator { map, .. } => on_child(*map),
            Self::RegExpStringIterator { regexp, .. } => on_child(*regexp),
            Self::Generator(state) => state.for_each_child(on_child),
            Self::Promise(data) => data.for_each_child(on_child),
            Self::Coroutine(coroutine) => {
                on_child(coroutine.promise);
                coroutine.state.for_each_child(on_child);
            }
            Self::Map(data) | Self::Set(data) | Self::WeakMap(data) | Self::WeakSet(data) => {
                data.for_each_child(on_child);
            }
            _ => {}
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        match self {
            Self::Function(function) => function.dec_ref_ids(stack),
            Self::ArrayIterator { array, .. } => stack.push(*array),
            Self::MapIterator { map, .. } => stack.push(*map),
            Self::RegExpStringIterator { regexp, .. } => stack.push(*regexp),
            Self::Generator(state) => state.dec_ref_ids(stack),
            Self::Promise(data) => data.dec_ref_ids(stack),
            Self::Coroutine(coroutine) => {
                stack.push(coroutine.promise);
                coroutine.state.dec_ref_ids(stack);
            }
            Self::Map(data) | Self::Set(data) | Self::WeakMap(data) | Self::WeakSet(data) => data.dec_ref_ids(stack),
            _ => {}
        }
    }

    /// Rebuilds derived state after deserialization.
    fn on_restore(&mut self) {
        if let Self::Map(data) | Self::Set(data) | Self::WeakMap(data) | Self::WeakSet(data) = self {
            data.rebuild_index();
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
    /// A host function: an undeclared name in call position. Calling it
    /// suspends the feed with a `FunctionCall` for the host to answer.
    External(AtomId),
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
            Self::Native(_) | Self::External(_) => {}
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
            Self::Native(_) | Self::External(_) => {}
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

    fn on_restore(&mut self, _: &mut JsHeapState, _: HeapId) {
        if let Self::Object(object) = self {
            object.class.on_restore();
        }
    }
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
