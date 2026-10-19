//! The heap itself: entries, reference counting, the collector's entry points
//! and dump serialization.

use std::{
    cell::{Cell, UnsafeCell},
    fmt,
};

use monty_types::{DEFAULT_GC_INTERVAL, ResourceTracker};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::{HeapId, HeapObject, HeapReader, StableHeap};

/// Reference-counted arena that backs all heap-only runtime values.
///
/// Uses a free list to reuse slots from freed values, keeping memory usage
/// constant for long-running loops that repeatedly allocate and free values.
/// When a value is freed via `dec_ref`, its slot ID is added to the free list.
/// New allocations pop from the free list when available, otherwise append.
///
/// Cycle collection uses Bacon–Rajan trial deletion: candidates come from
/// `dec_ref` (every container whose refcount drops to a non-zero value is
/// flagged [`Purple`](CcColor::Purple)), so the VM does not enumerate live
/// roots — refcount math itself proves reachability and values held only on
/// the Rust stack are correctly preserved by their non-zero refcount.
///
/// Owns the [`ResourceTracker`] that enforces memory/time limits and schedules
/// GC; unconfigured limits reduce every check to a single predictable branch.
#[derive(Debug)]
pub struct Heap<D: HeapObject> {
    /// Paged storage for heap entries with integrated free list.
    pub(crate) entries: StableHeap<HeapEntry<D>>,
    /// Resource tracker for enforcing limits and scheduling GC.
    pub tracker: ResourceTracker,
    /// Number of entries currently flagged [`Purple`](CcColor::Purple) — i.e.,
    /// suspected cycle roots awaiting collection.
    ///
    /// Used as an early-out: when zero, `collect_cycles` has no candidates
    /// and skips its heap walk entirely. The actual GC *frequency* is still
    /// driven by `allocations_since_gc` against the configured interval, so
    /// programs that produce no cycle candidates pay no collector cost
    /// regardless of how many allocations they perform.
    ///
    /// All `dec_ref` paths that mutate this counter take `&mut self`, so a
    /// plain `usize` is sufficient (no interior mutability needed).
    pub(crate) purple_count: usize,
    /// Number of GC-applicable allocations since the last cycle collection.
    ///
    /// Incremented for every GC-tracked allocation (see [`HeapObject::is_gc_tracked`])
    /// and reset to zero at the end of every successful [`collect_cycles`](Self::collect_cycles)
    /// call. Combined with [`purple_count`](Self::purple_count) to gate
    /// automatic collections in [`should_gc`](Self::should_gc).
    ///
    /// Uses `Cell` for interior mutability so that `allocate(&self)` can
    /// increment.
    pub(crate) allocations_since_gc: Cell<u32>,
    /// When true, [`should_gc`](Self::should_gc) returns false regardless of
    /// the candidate count, suppressing automatic cycle-collection passes.
    /// Toggled by the language's `gc.disable()` / `gc.enable()` test helpers.
    /// Explicit `collect_cycles` calls still run.
    #[cfg(feature = "test-hooks")]
    gc_disabled: bool,
    /// The payload type's per-heap state: singletons and weak indexes.
    extra: D::Extra,
}

impl<D: HeapObject> Heap<D> {
    /// Creates a new heap with the given resource tracker, holding the payload
    /// type's [`initial_entries`](HeapObject::initial_entries) from id zero.
    ///
    /// Use this to create heaps with custom resource limits or GC scheduling.
    #[must_use]
    pub fn new(capacity: usize, tracker: ResourceTracker) -> Self {
        let this = Self {
            entries: StableHeap::with_capacity(capacity),
            tracker,
            purple_count: 0,
            allocations_since_gc: Cell::new(0),
            #[cfg(feature = "test-hooks")]
            gc_disabled: false,
            extra: D::Extra::default(),
        };

        // A singleton starts with refcount = 1 — that single ref *is* the permanent
        // heap-owned reference. Hand-outs bump the refcount so callers can `dec_ref`
        // normally; the heap-owned ref keeps the singleton's rc ≥ 1 forever, which is
        // why trial deletion needs no special-case rooting for it. Singletons are not
        // counted against the GC interval: they are never garbage.
        for data in D::initial_entries() {
            this.entries.allocate(HeapEntry::new(data));
        }
        this
    }

    /// Number of entries in the heap (including freed slots).
    #[must_use]
    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// Whether `id` names a live (allocated, not freed) entry.
    #[must_use]
    pub fn is_live(&self, id: HeapId) -> bool {
        // SAFETY: [DH] - the slot reference is reduced to a `bool` before this
        // call returns, so no `&None` can overlap with a later `allocate()`.
        unsafe { self.entries.slot_at(id) }.is_some_and(Option::is_some)
    }

    /// Returns the number of GC-tracked allocations since the last cycle
    /// collection. Reset to zero by [`collect_cycles`](Self::collect_cycles).
    ///
    /// Exposes the GC trigger metric to reference-counting tests: a value much
    /// smaller than the configured `gc_interval` after many allocations is
    /// evidence that collection ran.
    #[cfg(feature = "ref-count-return")]
    #[must_use]
    pub fn get_allocations_since_gc(&self) -> u32 {
        self.allocations_since_gc.get()
    }

    /// Allocates a new heap entry.
    ///
    /// GC-tracked types bump `allocations_since_gc` so that
    /// [`should_gc`](Self::should_gc) eventually fires; trial deletion's own
    /// candidate enrollment happens later, at `dec_ref` time. Leaf types
    /// (strings, bytes, …) cannot participate in cycles and don't count
    /// against the GC interval.
    pub fn allocate(&self, data: D) -> HeapId {
        self.entries.allocate(self.new_entry(data))
    }

    /// Builds the initialized entry shared by typed and untyped allocation.
    pub(crate) fn new_entry(&self, data: D) -> HeapEntry<D> {
        if data.is_gc_tracked() {
            self.allocations_since_gc
                .set(self.allocations_since_gc.get().wrapping_add(1));
        }
        HeapEntry::new(data)
    }

    /// The payload type's per-heap state (singletons, weak indexes).
    #[must_use]
    pub fn extra(&self) -> &D::Extra {
        &self.extra
    }

    /// Mutable access to the payload type's per-heap state.
    pub fn extra_mut(&mut self) -> &mut D::Extra {
        &mut self.extra
    }

    /// Increments the reference count for an existing heap entry.
    ///
    /// # Panics
    /// Panics if the value ID is invalid or the value has already been freed.
    pub fn inc_ref(&self, id: HeapId) {
        let value = self.entries.get(id);
        value.refcount.update(|r| r + 1);
    }

    /// Decrements the reference count and frees the value (plus children) once it hits zero.
    ///
    /// This is the low-level release operation for an owned raw `HeapId`. Ordinary
    /// control flow should instead keep local ownership in the language's reference
    /// value and use `defer_drop!` or `DropGuard`. Heap-stored owners declare child
    /// references in [`HeapObject::dec_ref_ids`]; direct calls are appropriate in
    /// cleanup for other structures that own raw IDs.
    ///
    /// Uses an iterative work stack instead of recursion to avoid Rust stack overflow
    /// when freeing deeply nested containers (e.g., a list nested 10,000 levels deep).
    /// This is analogous to CPython's "trashcan" mechanism for safe deallocation.
    ///
    /// Implements the candidate-enrollment side of Bacon–Rajan trial deletion: any
    /// GC-tracked entry whose refcount survives the decrement gets flagged
    /// [`Purple`](CcColor::Purple), so the next [`collect_cycles`](Self::collect_cycles)
    /// can investigate it. Entries that drop to zero are freed immediately on the
    /// existing fast path; if such an entry was Purple, the heap-wide
    /// `purple_count` is rebalanced so it stays in sync with the actual number
    /// of Purple entries.
    ///
    /// # Panics
    /// Panics if the value ID is invalid, the value has already been freed, or
    /// the refcount would reach zero while active `HeapRead` readers exist.
    pub fn dec_ref(&mut self, id: HeapId) {
        HeapReader::with(self, &mut (), |reader, ()| {
            let mut current_id = id;
            // A fresh Vec is deliberate: it costs nothing unless children are
            // actually pushed, whereas pooling it on the Heap was measured
            // (CodSpeed, PR #536) to add take/restore traffic to every call.
            let mut work_stack = Vec::new();
            loop {
                // Using `HeapPtr` avoids the possibility of aliasing with live borrows
                // held by `HeapRead` handles.
                let ptr = reader.read_ptr(current_id);
                let heap_entry = ptr.entry(reader);
                if heap_entry.refcount.get() > 1 {
                    heap_entry.refcount.update(|r| r - 1);

                    let is_gc_tracked = ptr.data(reader).is_gc_tracked();
                    if is_gc_tracked && heap_entry.color.get() != CcColor::Purple {
                        // The refcount survived — a newly unreachable cycle could
                        // now be hiding. Flag it as a candidate for the next `collect_cycles`.
                        heap_entry.color.set(CcColor::Purple);
                        reader.heap.purple_count += 1;
                    }
                } else {
                    assert!(
                        heap_entry.readers.get() == 0,
                        "Heap::dec_ref: cannot free HeapId({}) with {} active reader(s)",
                        current_id.index(),
                        heap_entry.readers.get(),
                    );
                    // If the entry was a pending cycle candidate, decrement
                    // `purple_count` to reflect that it is leaving the heap before
                    // the collector reaches it.
                    if heap_entry.color.get() == CcColor::Purple {
                        reader.heap.purple_count -= 1;
                    }

                    // It is not possible to free from `HeapPtr` because it is created through
                    // a &self borrow on `StableHeap`. At least this repeated lookup is already
                    // on the slow path.
                    let mut value = reader.heap.entries.entry(current_id).expect("already looked up").free();
                    let data = value.data.0.get_mut();
                    // Clear weak-index entries before the slot becomes available for reuse.
                    data.on_free(&mut reader.heap.extra, current_id);
                    // Collect child IDs and push onto work stack for iterative processing
                    data.dec_ref_ids(&mut work_stack);
                }

                let Some(next_id) = work_stack.pop() else {
                    break;
                };
                current_id = next_id;
            }
        });
    }

    /// Returns an immutable reference to the heap data stored at the given ID. This can be more efficient
    /// than `.read()` for short-lived borrows that need read-only access (avoids reader bookkeeping).
    ///
    /// # Panics
    /// Panics if the value ID is invalid or the value has already been freed.
    #[must_use]
    pub fn get(&self, id: HeapId) -> &D {
        let data = &self.entries.get(id).data;
        // SAFETY: (DH) no mutable references into the data are possible while the heap is borrowed
        unsafe { &*data.0.get() }
    }

    /// Returns a mutable reference to the heap data stored at the given ID.
    ///
    /// Sound because every typed handle ([`HeapRead`](crate::HeapRead) and the
    /// pointers behind it) is tied to a [`HeapReader`] that holds the heap
    /// exclusively borrowed, so none can be alive while this `&mut self` borrow
    /// is; a handle created afterwards re-reads the entry's variant.
    ///
    /// # Panics
    /// Panics if the value ID is invalid or the value has already been freed.
    pub fn get_mut(&mut self, id: HeapId) -> &mut D {
        self.entries
            .entry(id)
            .expect("Heap::get_mut: entry already freed")
            .into_mut()
            .data
            .0
            .get_mut()
    }

    /// Returns the reference count for the heap entry at the given ID.
    ///
    /// This is primarily used for testing reference counting behavior.
    ///
    /// # Panics
    /// Panics if the value ID is invalid or the value has already been freed.
    #[must_use]
    #[cfg(feature = "ref-count-return")]
    pub fn get_refcount(&self, id: HeapId) -> usize {
        self.entries.get(id).refcount.get()
    }

    /// Returns the number of live (non-freed) entries on the heap, the payload
    /// type's [`initial_entries`](HeapObject::initial_entries) included.
    ///
    /// This is primarily used for testing to verify that all heap entries
    /// are accounted for in reference count tests.
    #[must_use]
    #[cfg(feature = "ref-count-return")]
    pub fn live_entry_count(&self) -> usize {
        self.entries.iter().count()
    }

    /// Number of entries currently flagged [`Purple`](CcColor::Purple): cycle
    /// candidates the next collection will investigate.
    #[must_use]
    pub fn purple_count(&self) -> usize {
        self.purple_count
    }

    /// Returns whether cycle collection should run.
    ///
    /// True when the configured allocation interval has elapsed *and* at
    /// least one [`Purple`](CcColor::Purple) candidate is pending. The
    /// alloc-count check sets the maximum collector frequency the user
    /// asked for; the `purple_count` check is an additional early-out so
    /// programs that produce no cycle candidates pay no collector cost
    /// regardless of their allocation rate.
    ///
    /// Always returns false when [`disable_gc`](Self::disable_gc) has been
    /// called without a matching [`enable_gc`](Self::enable_gc); explicit
    /// [`collect_cycles`](Self::collect_cycles) calls still run regardless.
    #[inline]
    #[must_use]
    pub fn should_gc(&self) -> bool {
        #[cfg(feature = "test-hooks")]
        if self.gc_disabled {
            return false;
        }
        if self.purple_count == 0 {
            return false;
        }
        let interval = self.tracker.gc_interval().unwrap_or(GC_INTERVAL);
        (self.allocations_since_gc.get() as usize) >= interval
    }

    /// Suppresses automatic garbage collection until [`enable_gc`](Self::enable_gc)
    /// is called.
    ///
    /// Explicit [`collect_cycles`](Self::collect_cycles) calls still run while
    /// disabled, so a script can build a known amount of garbage and then time
    /// exactly one collection pass.
    #[cfg(feature = "test-hooks")]
    pub fn disable_gc(&mut self) {
        self.gc_disabled = true;
    }

    /// Resumes automatic garbage collection after a prior [`disable_gc`](Self::disable_gc).
    ///
    /// Calling [`enable_gc`](Self::enable_gc) on an already-enabled heap is a no-op.
    #[cfg(feature = "test-hooks")]
    pub fn enable_gc(&mut self) {
        self.gc_disabled = false;
    }

    /// Runs Bacon–Rajan trial-deletion cycle collection.
    ///
    /// Walks every entry currently flagged [`Purple`](CcColor::Purple) (the
    /// candidates accumulated by `dec_ref`) and frees any references that turn
    /// out to live entirely inside an unreachable cycle. Refcount math itself
    /// proves liveness — entries reachable from outside the candidate set
    /// (including those held only on the Rust stack and those with active
    /// `HeapRead` readers) survive automatically because their refcount or
    /// reader count remains non-zero — so no explicit root walk is required.
    ///
    /// Phases:
    ///
    /// 1. **`MarkRoots`** — single linear pass over `entries` that finds
    ///    Purple entries, runs `MarkGray` on each, and collects the resulting
    ///    seed list. Purple entries reached transitively by an earlier seed's
    ///    `MarkGray` turn Gray and are correctly skipped, so each cycle root
    ///    is only seeded once.
    /// 2. **`Scan`** — for each seed, decide whether the subtree is alive
    ///    (`s.refcount > 0 || s.readers > 0`, resurrect to Black) or condemned
    ///    (mark White and recurse).
    /// 3. **`CollectWhite`** — free White entries iteratively. Child
    ///    refcounts were already balanced by `MarkGray`/`ScanBlack`, so this
    ///    phase does **not** call `dec_ref` on children — it only walks them
    ///    to free transitively.
    ///
    /// All phases iterate via explicit work stacks instead of recursion
    /// (the textbook formulation is recursive); a 10 000-deep nested cycle
    /// must collect without a Rust stack overflow.
    ///
    /// Returns the number of unreachable entries that were freed during the sweep.
    ///
    /// # Caller Responsibility
    /// The caller should check [`should_gc`](Self::should_gc) before calling
    /// this method. With `purple_count == 0` the function returns immediately
    /// without touching the heap.
    pub fn collect_cycles(&mut self) -> usize {
        if self.purple_count == 0 {
            return 0;
        }
        // The mark/scan phases work in terms of `HeapPtr<'a>` to avoid re-indexing
        // paged storage on every entry access; the brand `'a` comes from the
        // surrounding `HeapReader::with` scope.
        HeapReader::with(self, &mut (), |reader, ()| reader.collect_cycles_inner())
    }

    /// `CollectWhite`: frees every entry in `roots`' condemned subtrees.
    ///
    /// Kept in `HeapId` space because freeing a slot requires consulting the
    /// free list, which is keyed by id.
    pub(crate) fn collect_white(&mut self, roots: Vec<HeapId>) -> usize {
        let mut work_stack = roots;
        let mut freed = 0;
        while let Some(id) = work_stack.pop() {
            let Some(mut entry) = self.entries.entry(id) else {
                // Already freed via another seed's traversal — ignore.
                continue;
            };
            let heap_entry = entry.get_mut();
            if heap_entry.color.get() != CcColor::White {
                // Either resurrected to Black by `Scan` or never visited
                // (still Black/Gray from somewhere). Don't free.
                continue;
            }
            debug_assert!(
                heap_entry.readers.get() == 0,
                "collect_white: cannot free HeapId({}) with {} active reader(s)",
                id.index(),
                heap_entry.readers.get(),
            );
            let mut value = entry.free();
            let data = value.data.0.get_mut();
            // Clear weak entries before freeing their slots, just as `dec_ref`.
            data.on_free(&mut self.extra, id);
            freed += 1;
            // Walk children, letting the payload mark its released references
            // (`Value::Dereferenced` under `memory-model-checks`) so dropping the
            // freed entry's data doesn't trip a Drop-panic. The pushed child IDs
            // feed the work stack so we recursively walk White grandchildren — we
            // do *not* `dec_ref` these children (`MarkGray`/`ScanBlack` already
            // balanced their refcounts).
            data.dec_ref_ids(&mut work_stack);
        }
        freed
    }
}

impl<D: HeapObject> Serialize for Heap<D> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        /// Borrowed mirror of the heap's persisted fields; `extra` is flattened so
        /// the payload type's singletons keep their own names in the dump.
        #[derive(Serialize)]
        #[serde(bound = "D: HeapObject")]
        struct HeapRef<'a, D: HeapObject> {
            entries: &'a StableHeap<HeapEntry<D>>,
            tracker: &'a ResourceTracker,
            purple_count: usize,
            allocations_since_gc: u32,
            #[serde(flatten)]
            extra: &'a D::Extra,
        }

        HeapRef {
            entries: &self.entries,
            tracker: &self.tracker,
            purple_count: self.purple_count,
            allocations_since_gc: self.allocations_since_gc.get(),
            extra: &self.extra,
        }
        .serialize(serializer)
    }
}

impl<'de, D: HeapObject> Deserialize<'de> for Heap<D> {
    fn deserialize<De: Deserializer<'de>>(deserializer: De) -> Result<Self, De::Error> {
        /// Owned mirror of [`Heap`]'s persisted fields, see `HeapRef` in `Serialize`.
        #[derive(Deserialize)]
        #[serde(bound = "D: HeapObject")]
        struct HeapFields<D: HeapObject> {
            entries: StableHeap<HeapEntry<D>>,
            tracker: ResourceTracker,
            purple_count: usize,
            allocations_since_gc: u32,
            #[serde(flatten)]
            extra: D::Extra,
        }

        let mut fields = HeapFields::<D>::deserialize(deserializer)?;
        restore_entries(&mut fields.entries, &mut fields.extra).map_err(De::Error::custom)?;
        Ok(Self {
            entries: fields.entries,
            tracker: fields.tracker,
            purple_count: fields.purple_count,
            allocations_since_gc: Cell::new(fields.allocations_since_gc),
            #[cfg(feature = "test-hooks")]
            gc_disabled: false,
            extra: fields.extra,
        })
    }
}

/// Rebuilds weak indexes and rejects transient GC states before the heap can be used.
///
/// Snapshots are trusted for correctness, but must remain memory-safe even when invalid.
/// Gray/White entries would bypass parts of marking/scanning, on which the collector's
/// protection of live `HeapRead` pointers depends. Collection cannot suspend in these states.
fn restore_entries<D: HeapObject>(
    entries: &mut StableHeap<HeapEntry<D>>,
    extra: &mut D::Extra,
) -> Result<(), &'static str> {
    for index in 0..entries.len() {
        let id = HeapId::from_index(index);
        let Some(mut entry) = entries.entry(id) else {
            continue;
        };
        if matches!(entry.color.get(), CcColor::Gray | CcColor::White) {
            return Err("snapshot contains a transient GC color");
        }
        entry.get_mut().data.0.get_mut().on_restore(extra, id);
    }
    Ok(())
}

// With `memory-model-checks` enabled, need to manually clean up the heap to avoid the
// bookkeeping causing panics at shutdown.
#[cfg(feature = "memory-model-checks")]
impl<D: HeapObject> Drop for Heap<D> {
    fn drop(&mut self) {
        for id in 0..self.entries.len() {
            if let Some(mut entry) = self.entries.entry(HeapId::from_index(id)) {
                // Let the payload mark its references as released so they don't panic when dropped
                entry.get_mut().data.0.get_mut().dec_ref_ids(&mut Vec::new());
                entry.free();
            }
        }
    }
}

/// GC interval when the resource tracker sets none: [`DEFAULT_GC_INTERVAL`].
///
/// The trial-deletion collector additionally short-circuits the trace when
/// `purple_count == 0`, so programs that produce no cycle candidates pay no
/// collector cost regardless of their allocation rate.
///
/// When the `memory-model-checks` feature is enabled, this is reduced to 1 to
/// stress-test GC behavior on every allocation.
const GC_INTERVAL: usize = if cfg!(feature = "memory-model-checks") {
    1
} else {
    DEFAULT_GC_INTERVAL
};

/// A single entry inside the heap arena, storing refcount and payload.
///
/// Hashing state lives on the payload types that benefit from it; nothing here
/// is per-entry metadata beyond what reference counting and the collector need.
///
/// The `color` field encodes the entry's state for the trial-deletion cycle
/// collector (see [`CcColor`]). Outside of a running collection, every live
/// entry is either Black (uninteresting) or Purple (a cycle-root candidate
/// queued for investigation); Gray and White are transient states only seen
/// during [`Heap::collect_cycles`]. Cell-typed for symmetry with `refcount`
/// — every write goes through an `&mut Heap` path (`dec_ref` and the
/// collector's `mark_gray`/`scan`/`scan_black`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "D: HeapObject")]
pub struct HeapEntry<D: HeapObject> {
    #[serde(rename = "R")]
    pub(crate) refcount: Cell<usize>,
    /// Number of active `HeapRead` pointers into this entry's data.
    ///
    /// Incremented when `HeapReader::read` creates a `HeapRead`, decremented when
    /// the `HeapRead` is dropped. `dec_ref` panics if it would free an entry that
    /// still has active readers — this guarantees that `HeapRead` pointers remain
    /// valid for as long as they exist.
    #[serde(skip)] // should always be 0 during serde ops
    pub(crate) readers: Cell<usize>,
    /// The payload data
    #[serde(rename = "D")]
    pub(crate) data: UnsafeHeapData<D>,
    /// Cycle-collector color. See [`CcColor`].
    ///
    /// Round-trips through serde because a snapshot taken between bytecode
    /// instructions can capture entries in the [`Purple`](CcColor::Purple)
    /// pending-collection state; dropping the color on restore would leak
    /// any cycle that became unreachable just before the snapshot.
    #[serde(rename = "C")]
    pub(crate) color: Cell<CcColor>,
}

impl<D: HeapObject> HeapEntry<D> {
    /// A live entry holding one reference and no readers.
    fn new(data: D) -> Self {
        Self {
            refcount: Cell::new(1),
            readers: Cell::new(0),
            data: UnsafeHeapData(UnsafeCell::new(data)),
            color: Cell::new(CcColor::Black),
        }
    }
}

/// This wrapper containing `UnsafeCell` exists to allow for data inside a heap entry
/// to be safely pointed to via the `HeapReader` API.
///
/// The safety invariants are protected by the `Heap` / `HeapReader` API:
///   - It is never possible to alias mutable and immutable borrows into heap values,
///     whether they are the same or different value.
///   - When a mutable borrow of a heap value exists, no other heap value may be
///     borrowed. (See `Heap::get_mut` and `HeapRead::get`, which both require a `&mut`
///     borrow on the heap.)
pub(crate) struct UnsafeHeapData<D>(pub(crate) UnsafeCell<D>);

impl<D: fmt::Debug> fmt::Debug for UnsafeHeapData<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: (DH) Debug formatting is read-only and never called concurrently
        // with mutation. This matches the safety invariants of the HeapReader API.
        let data = unsafe { &*self.0.get() };
        f.debug_tuple("UnsafeHeapData").field(data).finish()
    }
}

impl<D: Serialize> Serialize for UnsafeHeapData<D> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // SAFETY: when heap data is being serialized, there is no mutable borrow
        // possible on any data contents
        D::serialize(unsafe { &*self.0.get() }, serializer)
    }
}

impl<'de, D: Deserialize<'de>> Deserialize<'de> for UnsafeHeapData<D> {
    fn deserialize<De: Deserializer<'de>>(deserializer: De) -> Result<Self, De::Error> {
        Ok(Self(UnsafeCell::new(D::deserialize(deserializer)?)))
    }
}

/// Color tag used by the trial-deletion cycle collector (Bacon–Rajan, ECOOP 2001).
///
/// Each [`HeapEntry`] carries a color that represents what the collector currently
/// believes about the entry. Outside of a running collection, every reachable
/// entry is either [`Black`](Self::Black) (live, not part of any suspected cycle)
/// or [`Purple`](Self::Purple) (a candidate cycle root discovered by `dec_ref`,
/// awaiting investigation). [`Gray`](Self::Gray) and [`White`](Self::White) are
/// transient states used only during a [`Heap::collect_cycles`] call.
///
/// The encoding fits in a single byte and is serialized as part of every
/// [`HeapEntry`]: a snapshot taken with cycles pending must round-trip through
/// serde so the entries stay enrolled as candidates after restore (otherwise a
/// graph that becomes garbage just before snapshot would leak permanently).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CcColor {
    /// Live and not currently a cycle candidate. Default state for every newly
    /// allocated entry.
    #[default]
    #[serde(rename = "B")]
    Black,
    /// Visited by `MarkGray` during a collection cycle. Children's refcounts
    /// have been provisionally decremented; a later `Scan` pass decides whether
    /// to resurrect (back to [`Black`](Self::Black)) or condemn
    /// ([`White`](Self::White)) the entry.
    #[serde(rename = "G")]
    Gray,
    /// Confirmed unreachable by the current collection: every reference into
    /// the entry comes from another condemned entry. `CollectWhite` will free
    /// it. Only seen mid-collection.
    #[serde(rename = "W")]
    White,
    /// Candidate cycle root. Set by `dec_ref` whenever a GC-tracked entry's
    /// refcount drops to a non-zero value — the only situation in which a new
    /// reference cycle can become unreachable. The collector seeds its work
    /// from every entry currently flagged Purple.
    #[serde(rename = "P")]
    Purple,
}
