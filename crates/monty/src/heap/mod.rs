//! The Python interpreter's instantiation of the shared heap.
//!
//! `monty_heap` owns the arena, reference counting, cycle collection and the
//! `HeapReader` access discipline; this module tells it what a Python object is
//! ([`HeapObject`] for [`HeapData`]), registers every payload type
//! ([`HeapReadOutput`], [`Payload`]), keeps the Python-only per-heap state
//! ([`PyHeapState`]: singletons and weak indexes) and adds the Python-flavoured
//! accessors as the [`HeapExt`] / [`HeapReaderExt`] extension traits.
//!
//! The typed handles ([`HeapRead`], [`HeapObjectRead`], [`BorrowedHeapRead`],
//! [`BorrowedHeapReadMut`]) are newtypes over `monty_heap`'s: the Python type
//! methods are inherent impls on them (`impl<'h> HeapRead<'h, List>` blocks
//! across `types/`), which the orphan rules only allow on a local type.

use std::{
    collections::BTreeMap,
    iter::once,
    marker::PhantomData,
    mem::size_of,
    ops::{Deref, DerefMut},
    sync::Arc,
};

pub(crate) use monty_heap::{HeapId, HeapObject, Payload, StableHeap};
use monty_types::MontyUuid;
use serde::{Deserialize, Serialize};

use crate::{
    asyncio::{Awaiter, ExternalFutureState, GatherState},
    types::{ExtFunction, HostClassType, TimeZone, Tuple},
    value::Value,
};
// Re-export items that live in `heap_traits` so that `crate::heap::DropGuard` etc. continue
// to resolve (used throughout the codebase).
pub(crate) use crate::{
    heap_data::HeapData,
    heap_traits::{ContainsHeap, DropGuard, DropWithContext, HeapItem, HeapPayload},
};

/// The Python heap: the shared arena holding [`HeapData`] payloads.
pub(crate) type Heap = monty_heap::Heap<HeapData>;
/// Scoped access to the Python heap; see [`monty_heap::HeapReader`].
pub(crate) type HeapReader<'a> = monty_heap::HeapReader<'a, HeapData>;
/// A branded pointer to a Python heap entry; see [`monty_heap::HeapPtr`].
pub(crate) type HeapPtr<'a> = monty_heap::HeapPtr<'a, HeapData>;
/// One Python heap entry: payload plus reference-count and collector bookkeeping.
pub(crate) type HeapEntry = monty_heap::HeapEntry<HeapData>;

/// What one heap entry costs, payload and bookkeeping together.
///
/// For preflighting a burst of allocations whose count is caller-controlled:
/// charging only the payloads would under-count by the refcount, reader count
/// and collector colour every entry also carries.
pub(crate) const HEAP_ENTRY_SIZE: usize = size_of::<HeapEntry>();

/// The empty tuple is a singleton which is allocated at startup.
pub(crate) const EMPTY_TUPLE_ID: HeapId = HeapId::from_index(0);

/// A typed, identity-free view protected by a [`HeapReader`]: `monty_heap::HeapRead`
/// carrying the Python type methods (`impl<'h> HeapRead<'h, T>` blocks in `types/`).
///
/// Derefs to the inner handle for `get` / `get_mut` / `as_slice` / `as_box_value`.
pub struct HeapRead<'a, T: ?Sized>(monty_heap::HeapRead<'a, T>);

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

impl<'a, T> HeapRead<'a, Vec<T>> {
    /// Reborrows the vector's contents as a slice handle.
    pub fn as_slice(&self, reader: &HeapReader<'a>) -> BorrowedHeapRead<'_, 'a, [T]> {
        self.0.as_slice(reader).into()
    }
}

impl<'a, T: ?Sized> HeapRead<'a, Box<T>> {
    /// Reborrows the box's contents as a handle to the boxed value.
    pub fn as_box_value(&self, reader: &HeapReader<'a>) -> BorrowedHeapRead<'_, 'a, T> {
        self.0.as_box_value(reader).into()
    }
}

/// A typed read handle for a complete Python object stored in a specific heap entry.
///
/// Unlike [`HeapRead`], this always represents the complete object rather than
/// protected external data or a projected field, so its [`HeapId`] is available
/// to operations that need Python identity or an owned reference to `self`.
pub struct HeapObjectRead<'a, T: ?Sized> {
    /// Identity of the complete Python object exposed by `read`.
    id: HeapId,
    /// Typed access guard that keeps the entry alive.
    read: HeapRead<'a, T>,
}

impl<'a, T: ?Sized> HeapObjectRead<'a, T> {
    /// Returns the heap entry containing this object.
    pub fn id(&self) -> HeapId {
        self.id
    }

    /// Returns a new owned reference to this object.
    pub fn clone_value(&self, heap: &HeapReader<'a>) -> Value {
        heap.inc_ref(self.id);
        Value::Ref(self.id)
    }
}

impl<'a, T: ?Sized> From<monty_heap::HeapObjectRead<'a, T>> for HeapObjectRead<'a, T> {
    fn from(read: monty_heap::HeapObjectRead<'a, T>) -> Self {
        let (id, read) = read.into_parts();
        Self {
            id,
            read: HeapRead(read),
        }
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

/// The reborrow of a handle as a field of the original type, or a view over
/// external data protected by the reader ([`HeapReaderExt::protect`]).
pub struct BorrowedHeapRead<'original, 'a, U: ?Sized> {
    inner: HeapRead<'a, U>,
    original: PhantomData<&'original U>,
}

impl<'original, 'a, U: ?Sized> From<monty_heap::BorrowedHeapRead<'original, 'a, U>>
    for BorrowedHeapRead<'original, 'a, U>
{
    fn from(read: monty_heap::BorrowedHeapRead<'original, 'a, U>) -> Self {
        Self {
            inner: HeapRead(read.into_inner()),
            original: PhantomData,
        }
    }
}

// NB no DerefMut - that is `BorrowedHeapReadMut`
impl<'a, U: ?Sized> Deref for BorrowedHeapRead<'_, 'a, U> {
    type Target = HeapRead<'a, U>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Mutable counterpart of [`BorrowedHeapRead`].
pub struct BorrowedHeapReadMut<'original, 'a, U: ?Sized> {
    inner: HeapRead<'a, U>,
    original: PhantomData<&'original mut U>,
}

impl<'original, 'a, U: ?Sized> From<monty_heap::BorrowedHeapReadMut<'original, 'a, U>>
    for BorrowedHeapReadMut<'original, 'a, U>
{
    fn from(read: monty_heap::BorrowedHeapReadMut<'original, 'a, U>) -> Self {
        Self {
            inner: HeapRead(read.into_inner()),
            original: PhantomData,
        }
    }
}

impl<'a, U: ?Sized> Deref for BorrowedHeapReadMut<'_, 'a, U> {
    type Target = HeapRead<'a, U>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<U: ?Sized> DerefMut for BorrowedHeapReadMut<'_, '_, U> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Reborrows a Python handle as a [`BorrowedHeapRead`] of one of `$ty`'s fields.
///
/// `$ty` must be a struct (never an enum or union) so the field has a fixed offset;
/// `$heap_read` is anything that coerces to `&HeapRead<'_, $ty>`.
macro_rules! heap_read_ref_as_field {
    ($heap_read:expr, $ty:ty, $field:tt) => {
        $crate::heap::BorrowedHeapRead::from(::monty_heap::heap_read_ref_as_field!($heap_read, $ty, $field))
    };
}

pub(crate) use heap_read_ref_as_field;

/// Mutable counterpart of [`heap_read_ref_as_field!`].
macro_rules! heap_read_ref_as_field_mut {
    ($heap_read:expr, $ty:ty, $field:tt) => {
        $crate::heap::BorrowedHeapReadMut::from(::monty_heap::heap_read_ref_as_field_mut!($heap_read, $ty, $field))
    };
}

pub(crate) use heap_read_ref_as_field_mut;

/// Python-side reader operations that hand out the local handle types.
pub(crate) trait HeapReaderExt<'a> {
    /// Wraps data that lives outside the heap in a [`HeapRead`]-shaped handle,
    /// so code written against heap handles can also run over a borrowed value.
    fn protect<'t, U: ?Sized>(&mut self, value: &'t U) -> BorrowedHeapRead<'t, 'a, U>;

    /// Mutable counterpart of [`protect`](Self::protect).
    fn protect_mut<'t, U: ?Sized>(&mut self, value: &'t mut U) -> BorrowedHeapReadMut<'t, 'a, U>;
}

impl<'a> HeapReaderExt<'a> for HeapReader<'a> {
    fn protect<'t, U: ?Sized>(&mut self, value: &'t U) -> BorrowedHeapRead<'t, 'a, U> {
        monty_heap::BorrowedHeapRead::protect(self, value).into()
    }

    fn protect_mut<'t, U: ?Sized>(&mut self, value: &'t mut U) -> BorrowedHeapReadMut<'t, 'a, U> {
        monty_heap::BorrowedHeapReadMut::protect(self, value).into()
    }
}

/// Python-specific heap operations: the singletons and the host-boundary indexes
/// kept in [`PyHeapState`].
pub(crate) trait HeapExt {
    /// Returns the singleton empty tuple.
    ///
    /// In Python, `() is ()` is always `True` because empty tuples are interned.
    /// This method provides the same optimization by returning the same `HeapId`
    /// for all empty tuple allocations.
    ///
    /// The returned `Value` has its reference count incremented, so the caller
    /// owns a reference and must call `dec_ref` when done.
    fn get_empty_tuple(&self) -> Value;

    /// Returns the external function for `name`, reusing a live object when possible.
    ///
    /// The cache holds no reference count of its own. Its entry is removed when the
    /// last owning reference is dropped, before the heap slot can be reused.
    fn get_ext_function(&mut self, name: &str) -> Value;

    /// Returns the cached `datetime.timezone.utc` singleton, lazily creating it on first access.
    ///
    /// The returned `Value::Ref` has its refcount incremented so the caller can drop
    /// it normally. The singleton itself is kept alive by the `timezone_utc` field.
    fn get_timezone_utc(&mut self) -> Value;

    /// Boundary uuid of the sandbox class or instance at `id`, generated and
    /// indexed on its first crossing to the host so the host can hand the
    /// object back by id (see [`HeapExt::resolve_boundary_uuid`]).
    ///
    /// # Panics
    /// If `id` is not a live `Instance` or `Class` entry.
    fn boundary_uuid(&mut self, id: HeapId) -> MontyUuid;

    /// The live sandbox class or instance that crossed to the host as `uuid`,
    /// if it still exists; the index holds no reference, so the returned id is
    /// borrowed.
    fn resolve_boundary_uuid(&self, uuid: &MontyUuid) -> Option<HeapId>;

    /// The live type object for the host class `uuid`, if the sandbox holds
    /// one; the index owns no reference, so the returned id is borrowed.
    fn resolve_host_type(&self, uuid: &MontyUuid) -> Option<HeapId>;

    /// Allocates and indexes the type object for a host class the sandbox has
    /// not seen yet; callers must check [`HeapExt::resolve_host_type`] first so
    /// the one-entry-per-class invariant holds.
    fn allocate_host_type(&mut self, ty: HostClassType) -> HeapId;

    /// Returns the number of live (non-freed) values on the heap, excluding the
    /// empty tuple singleton since it's an internal optimization detail that
    /// persists even when not explicitly used by user code.
    ///
    /// This is primarily used for testing to verify that all heap entries
    /// are accounted for in reference count tests.
    #[cfg(feature = "ref-count-return")]
    fn entry_count(&self) -> usize;
}

impl HeapExt for Heap {
    fn get_empty_tuple(&self) -> Value {
        // Return existing singleton with incremented refcount
        self.inc_ref(EMPTY_TUPLE_ID);
        Value::Ref(EMPTY_TUPLE_ID)
    }

    fn get_ext_function(&mut self, name: &str) -> Value {
        if let Some(id) = self.extra().ext_function_cache.get(name).copied() {
            self.inc_ref(id);
            Value::Ref(id)
        } else {
            let function = ExtFunction::new(name);
            let cache_key = function.cache_key();
            let id = self.allocate(HeapData::ExtFunction(function));
            let previous = self.extra_mut().ext_function_cache.insert(cache_key, id);
            debug_assert!(previous.is_none());
            Value::Ref(id)
        }
    }

    fn get_timezone_utc(&mut self) -> Value {
        if let Some(id) = self.extra().timezone_utc {
            self.inc_ref(id);
            Value::Ref(id)
        } else {
            let tz = TimeZone::utc();
            let id = self.allocate(HeapData::TimeZone(tz));
            // Keep an extra refcount for the singleton cache
            self.inc_ref(id);
            self.extra_mut().timezone_utc = Some(id);
            Value::Ref(id)
        }
    }

    fn boundary_uuid(&mut self, id: HeapId) -> MontyUuid {
        let uuid = match self.get_mut(id) {
            HeapData::Instance(instance) => instance.boundary_uuid(),
            HeapData::Class(class) => class.boundary_uuid(),
            _ => unreachable!("Heap::boundary_uuid: only classes and instances carry a boundary uuid"),
        };
        self.extra_mut().boundary_index.insert(uuid, id);
        uuid
    }

    fn resolve_boundary_uuid(&self, uuid: &MontyUuid) -> Option<HeapId> {
        self.extra().boundary_index.get(uuid).copied()
    }

    fn resolve_host_type(&self, uuid: &MontyUuid) -> Option<HeapId> {
        self.extra().host_type_index.get(uuid).copied()
    }

    fn allocate_host_type(&mut self, ty: HostClassType) -> HeapId {
        let uuid = ty.type_id();
        let id = self.allocate(HeapData::HostClassType(Box::new(ty)));
        let previous = self.extra_mut().host_type_index.insert(uuid, id);
        debug_assert!(
            previous.is_none(),
            "Heap::allocate_host_type: class {uuid} already has a type object"
        );
        id
    }

    #[cfg(feature = "ref-count-return")]
    fn entry_count(&self) -> usize {
        // The empty tuple singleton is always live, so it is the one entry excluded.
        self.live_entry_count().saturating_sub(1)
    }
}

/// Python's per-heap state beside the entries: the `timezone.utc` singleton and
/// the weak indexes that resolve host-facing ids back to live objects.
///
/// Only `timezone_utc` persists in a dump (flattened into the heap's own map, so
/// its name is unchanged); the indexes are rebuilt from the entries on restore.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct PyHeapState {
    /// Cached HeapId for the `datetime.timezone.utc` singleton.
    ///
    /// Lazily allocated on first access to `timezone.utc`. Once created, the refcount
    /// is incremented on each access so the caller can drop their reference normally.
    timezone_utc: Option<HeapId>,
    /// Live external functions indexed by name without owning heap references.
    ///
    /// Uses `BTreeMap` to avoid large residual capacity from spikes of `ExtFunction` allocations.
    #[serde(skip)]
    ext_function_cache: BTreeMap<Arc<str>, HeapId>,
    /// Live sandbox classes and instances by boundary uuid, so a value the host
    /// hands back resolves to the original object. Weak like
    /// `ext_function_cache`: cleared when the object is freed, rebuilt on restore.
    #[serde(skip)]
    boundary_index: BTreeMap<MontyUuid, HeapId>,
    /// Live host class type objects by class uuid — one `HostClassType` entry
    /// per host class, shared by its instances and `type(x)`. Separate from
    /// `boundary_index` because host and sandbox uuids are distinct namespaces.
    /// Weak like the others: cleared on free, rebuilt on restore.
    #[serde(skip)]
    host_type_index: BTreeMap<MontyUuid, HeapId>,
}

impl PyHeapState {
    /// Indexes `id` under `uuid` when the object has one (it has crossed to the host).
    fn index_boundary_uuid(&mut self, uuid: Option<MontyUuid>, id: HeapId) {
        if let Some(uuid) = uuid {
            self.boundary_index.insert(uuid, id);
        }
    }
}

/// Clears the weak index entry for `id` only if it still points at this exact
/// object (a restored dump may hold duplicates).
fn remove_weak_entry<K: Ord>(index: &mut BTreeMap<K, HeapId>, key: &K, id: HeapId) {
    if index.get(key) == Some(&id) {
        index.remove(key);
    }
}

impl HeapObject for HeapData {
    type Extra = PyHeapState;
    type Ref = Value;
    type Read<'a>
        = HeapReadOutput<'a>
    where
        Self: 'a;

    fn initial_entries() -> Vec<Self> {
        vec![Self::Tuple(Tuple::default())]
    }

    fn make_ref(id: HeapId) -> Value {
        Value::Ref(id)
    }

    fn read<'a>(ptr: HeapPtr<'a>, id: HeapId, reader: &HeapReader<'a>) -> HeapReadOutput<'a> {
        read_heap_data(ptr, id, reader)
    }

    fn is_gc_tracked(&self) -> bool {
        // Resolves to the inherent `HeapData::is_gc_tracked` (inherent methods take
        // priority), which matches every variant exhaustively.
        Self::is_gc_tracked(self)
    }

    /// Walks the GC-relevant children of a value and calls `on_child` for each
    /// contained `HeapId`.
    ///
    /// The cycle collector's mark/scan phases use this directly, combining the
    /// child-id iteration with a `HeapId → HeapPtr` conversion in the closure
    /// body — that's why this is closure-shaped rather than producing a `Vec`.
    /// [`dec_ref_ids`](Self::dec_ref_ids) mirrors the same match arms for the
    /// freeing/decref paths; the two must stay in sync.
    fn for_each_child_id(&self, mut on_child: impl FnMut(HeapId)) {
        match self {
            Self::List(list) => {
                // Skip iteration if no refs - major GC optimization for lists of primitives
                if !list.contains_refs() {
                    return;
                }
                for value in list.as_slice() {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
            }
            // MUST report exactly the same ids as `Deque::py_dec_ref_ids` — reporting
            // fewer leaks, reporting more is a use-after-free.
            Self::Deque(deque) => {
                if !deque.contains_refs() {
                    return;
                }
                for value in deque.iter() {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
            }
            Self::Tuple(tuple) => {
                // Skip iteration if no refs - GC optimization for tuples of primitives
                if !tuple.contains_refs() {
                    return;
                }
                for value in tuple.as_slice() {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
            }
            Self::NamedTuple(nt) => {
                // Report the owned reference to the class object (factory instances
                // only) before the `contains_refs` early-out — that flag is computed
                // from `items` alone, so an all-primitive instance like `Point(1, 2)`
                // still has a live class edge. MUST report exactly the same ids as
                // `NamedTuple::py_dec_ref_ids`.
                if let Some(class_id) = nt.class_id() {
                    on_child(class_id);
                }
                // Skip iteration if no refs - GC optimization for namedtuples of primitives
                if !nt.contains_refs() {
                    return;
                }
                for value in nt.as_vec() {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
            }
            // A namedtuple *class* owns its default values and its `__module__`.
            // MUST report the same ids as `NamedTupleClass::py_dec_ref_ids`.
            Self::NamedTupleClass(class) => {
                if !class.contains_refs() {
                    return;
                }
                for value in class.defaults().iter().chain(once(class.module())) {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
            }
            Self::Dict(dict) => {
                // Report the default_factory (a defaultdict with a heap-ref factory,
                // e.g. a lambda) before the `has_refs` early-out. MUST report exactly
                // the same ids as `Dict::py_dec_ref_ids`: reporting fewer leaks,
                // reporting more is a use-after-free.
                if let Some(Value::Ref(id)) = dict.default_factory() {
                    on_child(*id);
                }
                // Skip iteration if no refs - major GC optimization for dicts of primitives
                if !dict.has_refs() {
                    return;
                }
                for (k, v) in dict {
                    if let Value::Ref(id) = k {
                        on_child(*id);
                    }
                    if let Value::Ref(id) = v {
                        on_child(*id);
                    }
                }
            }
            Self::DictKeysView(view) => {
                on_child(view.dict_id());
            }
            Self::DictItemsView(view) => {
                on_child(view.dict_id());
            }
            Self::DictValuesView(view) => {
                on_child(view.dict_id());
            }
            Self::Set(set) => {
                for value in set.storage().iter() {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
            }
            Self::FrozenSet(frozenset) => {
                for value in frozenset.storage().iter() {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
            }
            Self::Closure(closure) => {
                // Add captured cells to work list
                for cell_id in &closure.cells {
                    on_child(*cell_id);
                }
                // Add default values that are heap references
                for default in &closure.defaults {
                    if let Value::Ref(id) = default {
                        on_child(*id);
                    }
                }
                if let Some(globals) = closure.globals {
                    on_child(globals);
                }
            }
            Self::FunctionDefaults(fd) => {
                // Add default values that are heap references
                for default in &fd.defaults {
                    if let Value::Ref(id) = default {
                        on_child(*id);
                    }
                }
                if let Some(globals) = fd.globals {
                    on_child(globals);
                }
            }
            Self::Cell(cell) => {
                // Cell can contain a reference to another heap value
                if let Value::Ref(id) = &cell.0 {
                    on_child(*id);
                }
            }
            Self::HostClass(dc) => {
                // The owned class entry, then the attrs Dict — MUST report exactly
                // the same ids as `HostClass::py_dec_ref_ids`.
                on_child(dc.class_id());
                for (k, v) in dc.attrs() {
                    if let Value::Ref(id) = k {
                        on_child(*id);
                    }
                    if let Value::Ref(id) = v {
                        on_child(*id);
                    }
                }
            }
            Self::HostClassType(t) => {
                // Eager class attrs can hold containers the sandbox can reach
                // (`Klass.data`) and mutate to close a cycle back to this type
                // object, so they must be traced like `HostClass` attrs.
                for (k, v) in t.attrs() {
                    if let Value::Ref(id) = k {
                        on_child(*id);
                    }
                    if let Value::Ref(id) = v {
                        on_child(*id);
                    }
                }
            }
            Self::Class(class) => {
                // The class namespace holds method/class-variable values.
                for (k, v) in class.namespace() {
                    if let Value::Ref(id) = k {
                        on_child(*id);
                    }
                    if let Value::Ref(id) = v {
                        on_child(*id);
                    }
                }
            }
            Self::Instance(instance) => {
                // An instance references its class plus its attribute dict's entries.
                on_child(instance.class());
                for (k, v) in instance.attrs() {
                    if let Value::Ref(id) = k {
                        on_child(*id);
                    }
                    if let Value::Ref(id) = v {
                        on_child(*id);
                    }
                }
            }
            Self::BoundMethod(bm) => {
                if let Value::Ref(id) = &bm.instance {
                    on_child(*id);
                }
                if let Value::Ref(id) = &bm.func {
                    on_child(*id);
                }
            }
            Self::DataclassField(field) => {
                // A captured default or factory can reach back to the class the
                // field belongs to (`x: object = SomeInstanceOfIt`, or a factory
                // closure capturing the class), closing a cycle.
                for value in field.ref_children() {
                    on_child(value);
                }
            }
            Self::ListIterator(iter) => on_child(iter.list_id()),
            Self::DequeIterator(iter) => on_child(iter.deque_id()),
            Self::TupleIterator(iter) => on_child(iter.source_id()),
            Self::StringIterator(iter) => {
                if let Some(id) = iter.source_id() {
                    on_child(id);
                }
            }
            Self::BytesIterator(iter) => {
                if let Some(id) = iter.source_id() {
                    on_child(id);
                }
            }
            Self::RangeIterator(_) => {}
            Self::DictKeyIterator(iter) => on_child(iter.source_id()),
            Self::DictItemIterator(iter) => on_child(iter.source_id()),
            Self::DictValueIterator(iter) => on_child(iter.source_id()),
            Self::SetIterator(iter) => on_child(iter.source_id()),
            Self::CallableIterator(iter) => iter.for_each_child_id(on_child),
            Self::DirScan(scan) => scan.for_each_child_id(on_child),
            Self::Itertools(iter) => iter.for_each_child_id(on_child),
            Self::Csv(csv) => csv.for_each_child_id(on_child),
            Self::Partial(partial) => partial.for_each_child_id(on_child),
            Self::GenericAlias(alias) => alias.for_each_child_id(on_child),
            Self::Union(union) => union.for_each_child_id(on_child),
            Self::Module(m) => {
                // Module attrs can contain references to heap values
                if !m.has_refs() {
                    return;
                }
                for (k, v) in m.attrs() {
                    if let Value::Ref(id) = k {
                        on_child(*id);
                    }
                    if let Value::Ref(id) = v {
                        on_child(*id);
                    }
                }
            }
            Self::Coroutine(coro) => {
                // Add namespace values that are heap references
                for value in &coro.namespace {
                    if let Value::Ref(id) = value {
                        on_child(*id);
                    }
                }
                if let Some(globals) = coro.globals {
                    on_child(globals);
                }
            }
            Self::GatherFuture(gather) => {
                // Add inc_ref'd item HeapIds. Both coroutines and external
                // futures are owned by the gather for its entire lifecycle.
                for item in &gather.items {
                    on_child(*item);
                }
                // Walk per-state heap refs: in-flight slot results plus this
                // gather's own awaiter (if `GatherSlot`, it owns an inc_ref on
                // the outer gather), or the cached result list once the gather
                // has completed successfully. `Pending` and `Failed` carry no
                // heap refs.
                match &gather.state {
                    GatherState::Awaited(awaited) => {
                        if let Awaiter::GatherSlot { gather, .. } = &awaited.awaiter {
                            on_child(*gather);
                        }
                        for result in awaited.results.iter().flatten() {
                            if let Value::Ref(id) = result {
                                on_child(*id);
                            }
                        }
                    }
                    GatherState::Completed(Value::Ref(id)) => on_child(*id),
                    GatherState::Pending | GatherState::Failed(_) | GatherState::Completed(_) => {}
                }
            }
            Self::ExternalFuture(fut) => {
                // `Pending { awaiter: Some(GatherSlot { gather, .. }) }` owns an
                // inc_ref on `gather`. `Awaiter::Task` / `None` and the `Failed`
                // state carry no heap refs. `Resolved` owns the cached value, and
                // a pending sleep owns the `result` it will resolve with.
                if let Some(Value::Ref(id)) = &fut.sleep_result {
                    on_child(*id);
                }
                match &fut.state {
                    ExternalFutureState::Resolved(Value::Ref(id)) => on_child(*id),
                    ExternalFutureState::Pending {
                        awaiter: Some(Awaiter::GatherSlot { gather, .. }),
                    } => on_child(*gather),
                    _ => {}
                }
            }
            Self::DateTime(dt) => {
                // Aware datetimes retain a heap reference to the tzinfo object so that
                // `dt.tzinfo is tz` identity is preserved across attribute lookups.
                // GC must follow that reference, otherwise the timezone gets swept
                // while the datetime still points at the freed slot.
                if let Some(tz_id) = dt.tzinfo_ref() {
                    on_child(tz_id);
                }
            }
            Self::Time(t) => {
                // Same retained-tzinfo contract as `DateTime` above.
                if let Some(tz_id) = t.tzinfo_ref() {
                    on_child(tz_id);
                }
            }
            Self::OpenFile(file) => {
                // Kept in sync with `dec_ref_ids`: the file owns one ref on its
                // loaded buffer. (`OpenFile` is not GC-tracked today, so this arm is
                // not reached by the collector, but the two walkers must mirror each
                // other per the contract above.)
                if let Some(buffer_id) = file.buffer_id() {
                    on_child(buffer_id);
                }
            }
            Self::ReMatch(m) => {
                // Mirror `dec_ref_ids`: a match holds one ref on its shared subject
                // string (`None` for an interned subject).
                if let Value::Ref(id) = m.subject_ref() {
                    on_child(*id);
                }
            }
            // Leaf types with no heap references
            _ => {}
        }
    }

    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        match self {
            Self::Str(s) => s.py_dec_ref_ids(stack),
            Self::Bytes(b) => b.py_dec_ref_ids(stack),
            Self::List(l) => l.py_dec_ref_ids(stack),
            Self::Deque(d) => d.py_dec_ref_ids(stack),
            Self::Tuple(t) => t.py_dec_ref_ids(stack),
            Self::NamedTuple(nt) => nt.py_dec_ref_ids(stack),
            Self::NamedTupleClass(class) => class.py_dec_ref_ids(stack),
            Self::Dict(d) => d.py_dec_ref_ids(stack),
            Self::DictKeysView(view) => view.py_dec_ref_ids(stack),
            Self::DictItemsView(view) => view.py_dec_ref_ids(stack),
            Self::DictValuesView(view) => view.py_dec_ref_ids(stack),
            Self::Set(s) => s.py_dec_ref_ids(stack),
            Self::FrozenSet(fs) => fs.py_dec_ref_ids(stack),
            Self::Closure(closure) => {
                // Decrement ref count for captured cells
                stack.extend(closure.cells.iter().copied());
                // Decrement ref count for default values that are heap references
                for default in &mut closure.defaults {
                    default.py_dec_ref_ids(stack);
                }
                stack.extend(closure.globals);
            }
            Self::FunctionDefaults(fd) => {
                // Decrement ref count for default values that are heap references
                for default in &mut fd.defaults {
                    default.py_dec_ref_ids(stack);
                }
                stack.extend(fd.globals);
            }
            Self::Cell(cell) => cell.0.py_dec_ref_ids(stack),
            Self::HostClass(dc) => dc.py_dec_ref_ids(stack),
            Self::HostClassType(t) => t.py_dec_ref_ids(stack),
            Self::Class(class) => class.py_dec_ref_ids(stack),
            Self::Instance(instance) => instance.py_dec_ref_ids(stack),
            Self::BoundMethod(bm) => bm.py_dec_ref_ids(stack),
            Self::DataclassField(field) => field.py_dec_ref_ids(stack),
            Self::DataclassParams(params) => params.py_dec_ref_ids(stack),
            Self::ListIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::DequeIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::TupleIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::StringIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::BytesIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::RangeIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::DictKeyIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::DictItemIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::DictValueIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::SetIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::CallableIterator(iter) => iter.py_dec_ref_ids(stack),
            Self::DirScan(scan) => scan.py_dec_ref_ids(stack),
            Self::Itertools(iter) => iter.py_dec_ref_ids(stack),
            Self::Csv(csv) => csv.py_dec_ref_ids(stack),
            Self::Partial(partial) => partial.py_dec_ref_ids(stack),
            Self::GenericAlias(alias) => alias.py_dec_ref_ids(stack),
            Self::Union(union) => union.py_dec_ref_ids(stack),
            Self::Module(m) => m.py_dec_ref_ids(stack),
            Self::Coroutine(coro) => {
                // Decrement ref count for namespace values that are heap references
                for value in &mut coro.namespace {
                    value.py_dec_ref_ids(stack);
                }
                stack.extend(coro.globals);
            }
            Self::GatherFuture(gather) => {
                // Decrement ref count for owned item HeapIds (coroutines and
                // external futures are both owned by the gather).
                stack.extend(gather.items.iter().copied());
                // Release per-state heap refs: in-flight slot results plus this
                // gather's own awaiter (if `GatherSlot`, it owns an inc_ref on
                // the outer gather), or the cached result list once the gather
                // has completed successfully. `Pending` and `Failed` carry no
                // heap refs.
                match &mut gather.state {
                    GatherState::Awaited(awaited) => {
                        if let Awaiter::GatherSlot { gather, .. } = &awaited.awaiter {
                            stack.push(*gather);
                        }
                        for result in awaited.results.iter_mut().flatten() {
                            result.py_dec_ref_ids(stack);
                        }
                    }
                    GatherState::Completed(value) => value.py_dec_ref_ids(stack),
                    GatherState::Pending | GatherState::Failed(_) => {}
                }
            }
            Self::ExternalFuture(fut) => {
                // Mirror `for_each_child_id`: a pending sleep's `result` is owned too.
                if let Some(result) = &mut fut.sleep_result {
                    result.py_dec_ref_ids(stack);
                }
                match &mut fut.state {
                    ExternalFutureState::Resolved(value) => value.py_dec_ref_ids(stack),
                    ExternalFutureState::Pending {
                        awaiter: Some(Awaiter::GatherSlot { gather, .. }),
                    } => stack.push(*gather),
                    ExternalFutureState::Pending {
                        awaiter: None | Some(Awaiter::Task(_)),
                    }
                    | ExternalFutureState::Failed(_) => {}
                }
            }
            Self::DateTime(dt) => {
                // Mirror `for_each_child_id`: when an aware datetime is freed we must
                // also drop the retained tzinfo reference so its refcount is balanced.
                if let Some(tz_id) = dt.tzinfo_ref() {
                    stack.push(tz_id);
                }
            }
            Self::Time(t) => {
                // Mirror `for_each_child_id`: an aware time owns its tzinfo reference.
                if let Some(tz_id) = t.tzinfo_ref() {
                    stack.push(tz_id);
                }
            }
            Self::OpenFile(f) => {
                // Kept in sync with `for_each_child_id`: release the file's owned
                // ref on its loaded buffer when the file is freed (e.g. read but
                // never `close()`d).
                f.py_dec_ref_ids(stack);
            }
            // Release the shared subject reference (mirrors `for_each_child_id`).
            Self::ReMatch(m) => m.py_dec_ref_ids(stack),
            // other types have no nested heap references
            _ => {}
        }
    }

    fn on_free(&mut self, extra: &mut PyHeapState, id: HeapId) {
        debug_assert!(
            id != EMPTY_TUPLE_ID,
            "Heap::dec_ref: empty-tuple singleton's heap-owned refcount must never reach zero",
        );
        match self {
            Self::ExtFunction(function) => {
                remove_weak_entry(&mut extra.ext_function_cache, &function.cache_key(), id);
            }
            Self::Instance(instance) => {
                if let Some(uuid) = instance.uuid() {
                    remove_weak_entry(&mut extra.boundary_index, &uuid, id);
                }
            }
            Self::Class(class) => {
                if let Some(uuid) = class.uuid() {
                    remove_weak_entry(&mut extra.boundary_index, &uuid, id);
                }
            }
            Self::HostClassType(ty) => remove_weak_entry(&mut extra.host_type_index, &ty.type_id(), id),
            _ => {}
        }
    }

    fn on_restore(&mut self, extra: &mut PyHeapState, id: HeapId) {
        match self {
            Self::ExtFunction(function) => {
                extra.ext_function_cache.insert(function.cache_key(), id);
            }
            Self::Instance(instance) => extra.index_boundary_uuid(instance.uuid(), id),
            Self::Class(class) => extra.index_boundary_uuid(class.uuid(), id),
            Self::HostClassType(ty) => {
                extra.host_type_index.insert(ty.type_id(), id);
            }
            _ => {}
        }
    }
}

macro_rules! heap_storage_value {
    (inline $value:expr) => {
        $value
    };
    (boxed $value:expr) => {
        Box::new($value)
    };
}

macro_rules! heap_project {
    (inline $ptr:expr, $id:expr, $reader:expr, $value:expr) => {
        $ptr.project($id, $reader, $value)
    };
    (boxed $ptr:expr, $id:expr, $reader:expr, $value:expr) => {
        $ptr.project_boxed($id, $reader, $value)
    };
}

macro_rules! define_heap_read_support {
    ($(
        $(#[$meta:meta])*
        $variant:ident($storage:ident $payload:ty)
    ),* $(,)?) => {
        /// A type-safe read handle for any payload stored in the heap.
        /// Variants mirror `HeapData`, which carries their docs; its serde attributes
        /// would not compile here, so the registry's attributes are not repeated.
        pub enum HeapReadOutput<'a> {
            $(
                $variant(HeapObjectRead<'a, $payload>),
            )*
        }

        impl HeapReadOutput<'_> {
            /// Returns the heap entry containing the complete dynamically read object.
            pub fn id(&self) -> HeapId {
                match self {
                    $(Self::$variant(value) => value.id(),)*
                }
            }
        }

        $(
            impl Payload<HeapData> for $payload {
                type Handle<'a>
                    = HeapObjectRead<'a, $payload>
                where
                    Self: 'a;

                #[inline]
                fn into_data(self) -> HeapData {
                    HeapData::$variant(heap_storage_value!($storage self))
                }

                #[inline]
                fn try_from_read<'a>(
                    read: HeapReadOutput<'a>,
                ) -> Result<HeapObjectRead<'a, Self>, HeapReadOutput<'a>>
                where
                    Self: 'a,
                    HeapData: 'a,
                {
                    if let HeapReadOutput::$variant(value) = read {
                        Ok(value)
                    } else {
                        Err(read)
                    }
                }
            }
        )*

        /// Projects the live variant at `ptr` into its typed read handle.
        fn read_heap_data<'a>(ptr: HeapPtr<'a>, id: HeapId, reader: &HeapReader<'a>) -> HeapReadOutput<'a> {
            match ptr.data(reader) {
                $(
                    HeapData::$variant(value) => HeapReadOutput::$variant(
                        heap_project!($storage ptr, id, reader, value).into()
                    ),
                )*
            }
        }
    };
}

crate::heap_data::heap_payloads!(define_heap_read_support);

/// Compile-fail soundness tests for [`HeapReader`].
///
/// Gated behind `--cfg heap_reader_compile_fail_tests` so they are only compiled
/// when the integration test harness runs `cargo check` with the appropriate flags.
#[cfg(heap_reader_compile_fail_tests)]
#[path = "../../tests/heap_reader_compile_fail_cases/cases.rs"]
mod heap_reader_compile_fail_cases;

/// Python-side collector tests: the collector itself is tested in `monty_heap`,
/// these cover what this module adds — the empty-tuple singleton and the
/// child-tracing of a payload whose edges are not plain `Value::Ref` items.
#[cfg(test)]
mod tests {
    use monty_types::ResourceTracker;

    use super::*;
    use crate::{
        types::{List, callable_iterator::CallableIterator},
        value::Value,
    };

    #[test]
    fn empty_tuple_singleton_survives_collection() {
        // The empty-tuple singleton is not rooted explicitly by the collector.
        // Its refcount stays ≥ 1 forever (initial heap-owned ref), which is what
        // keeps it alive — verify the collector does not accidentally free it
        // even after spurious Purple flagging.
        let mut heap = Heap::new(16, ResourceTracker::default());
        // Fake a dec_ref event that would mark the empty tuple Purple.
        heap.inc_ref(EMPTY_TUPLE_ID);
        heap.dec_ref(EMPTY_TUPLE_ID);
        heap.collect_cycles();
        assert!(
            heap.is_live(EMPTY_TUPLE_ID),
            "empty tuple singleton must survive collection"
        );
    }

    /// The GC must see BOTH refs a `callable_iterator` owns, with correct
    /// multiplicity: under-tracing here would let a live object be collected.
    #[test]
    fn callable_iterator_traces_callable_and_sentinel() {
        let heap = Heap::new(16, ResourceTracker::default());
        let c = heap.allocate(HeapData::List(List::new(vec![])));
        let s = heap.allocate(HeapData::List(List::new(vec![])));

        heap.inc_ref(c);
        heap.inc_ref(s);
        let iter = heap.allocate(HeapData::CallableIterator(CallableIterator::new(
            Value::Ref(c),
            Value::Ref(s),
        )));

        let mut traced = vec![];
        heap.get(iter).for_each_child_id(|id| traced.push(id));
        assert_eq!(traced, vec![c, s], "callable and sentinel are both traced");

        // Multiplicity: when callable IS sentinel, two counted refs point at one
        // object, so the id must be reported twice or trial deletion
        // under-decrements and frees a live object.
        heap.inc_ref(c);
        heap.inc_ref(c);
        let shared = heap.allocate(HeapData::CallableIterator(CallableIterator::new(
            Value::Ref(c),
            Value::Ref(c),
        )));

        let mut dup = vec![];
        heap.get(shared).for_each_child_id(|id| dup.push(id));
        assert_eq!(dup, vec![c, c], "a shared callable/sentinel is traced twice");
    }

    /// End-to-end: a cycle through the callable must be collected and the
    /// non-cycle sentinel released — exercising the mark phase
    /// (`for_each_child_id`) and the free phase (`dec_ref_ids`) together.
    #[test]
    fn callable_iterator_cycle_is_collected() {
        let mut heap = Heap::new(16, ResourceTracker::default());
        let sentinel = heap.allocate(HeapData::List(List::new(vec![])));
        let list = heap.allocate(HeapData::List(List::new(vec![])));
        heap.inc_ref(list);
        heap.inc_ref(sentinel);
        let iter = heap.allocate(HeapData::CallableIterator(CallableIterator::new(
            Value::Ref(list),
            Value::Ref(sentinel),
        )));

        // Close the cycle: the callable list references the iterator back.
        heap.inc_ref(iter);
        HeapReader::with(&mut heap, &mut (), |reader, ()| {
            let HeapReadOutput::List(mut l) = reader.read(list) else {
                unreachable!("just allocated a list")
            };
            let l = l.get_mut(reader);
            l.set_contains_refs();
            l.as_vec_mut().push(Value::Ref(iter));
        });
        // list.rc = 2, iter.rc = 2, sentinel.rc = 2.

        // Drop the allocation refs: {iter, list} is now an unreachable cycle and
        // the sentinel is held only by the iterator.
        heap.dec_ref(list);
        heap.dec_ref(iter);
        heap.dec_ref(sentinel);

        heap.collect_cycles();

        assert!(!heap.is_live(iter), "callable_iterator in a cycle must be collected");
        assert!(!heap.is_live(list), "the cycle's other node must be collected");
        assert!(
            !heap.is_live(sentinel),
            "sentinel held only by the freed iterator must be released"
        );
    }
}
