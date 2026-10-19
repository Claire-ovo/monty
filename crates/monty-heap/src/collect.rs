//! The trial-deletion collector's mark and scan phases, and the leak detector
//! built on the same traversal.
//!
//! Implemented on [`HeapReader<'a>`] so the work stacks can carry
//! [`HeapPtr<'a>`] instead of [`HeapId`]: once an entry is reached, every
//! subsequent access is a pointer deref rather than a paged-storage lookup.
//! Entered only via [`Heap::collect_cycles`](crate::Heap::collect_cycles), which
//! establishes the brand `'a` through [`HeapReader::with`].
//! [`Heap::collect_white`](crate::Heap::collect_white) is intentionally kept in
//! `HeapId` space because freeing a slot requires consulting the free list, which
//! is keyed by id.

#[cfg(feature = "ref-count-return")]
use std::collections::HashSet;

use crate::{CcColor, HeapId, HeapObject, HeapPtr, HeapReader};

impl<'a, D: HeapObject> HeapReader<'a, D> {
    /// Implementation of [`Heap::collect_cycles`](crate::Heap::collect_cycles).
    /// Phase 1 (discover Purple roots and `mark_gray` their subtrees), Phase 2
    /// (`scan`), and Phase 3 (`collect_white`) run sequentially. After `scan` the
    /// `HeapPtr` work stack is drained, so no branded pointer escapes into Phase 3.
    ///
    /// Each phase iterates [`HeapObject::for_each_child_id`] inline rather than
    /// staging child `HeapId`s through a scratch `Vec`. Mutation of entry fields
    /// goes through the `Cell`/`UnsafeCell`-backed interior mutability on
    /// `HeapEntry`, so the `&HeapEntry`/`&D` borrows from
    /// [`HeapPtr::entry`]/[`HeapPtr::data`] can coexist with the `read_ptr` calls
    /// in the closure body.
    pub(crate) fn collect_cycles_inner(&mut self) -> usize {
        let mut roots: Vec<HeapId> = Vec::new();
        let mut work_stack: Vec<HeapPtr<'a, D>> = Vec::new();

        // 1. Discover roots by finding Purple entries. Mark each root (and its subtree) Gray.
        //    The linear scan may legitimately encounter freed slots, so we use
        //    `try_entry` to skip them rather than panicking.
        for i in 0..self.heap.entries.len() {
            let id = HeapId::from_index(i);
            let ptr = self.read_ptr(id);
            let Some(entry) = ptr.try_entry(self) else {
                continue;
            };
            if entry.color.get() != CcColor::Purple {
                continue;
            }
            if entry.readers.get() > 0 {
                // This entry cannot possibly be a root since it has active readers; reset
                // to Black so it won't be a candidate in the next cycle.
                entry.color.set(CcColor::Black);
                continue;
            }
            roots.push(id);
            self.mark_gray(ptr, &mut work_stack);
            debug_assert!(work_stack.is_empty(), "mark_gray must drain its work stack");
        }

        // 2. For each root, scan and resurrect if alive (refcount > 0 or active readers).
        //    Roots were live at Phase 1 and nothing has freed them between then and
        //    now, so `read_ptr` returning a freed slot would indicate a bug.
        work_stack.extend(roots.iter().map(|&id| self.read_ptr(id)));
        self.scan(&mut work_stack);
        debug_assert!(work_stack.is_empty(), "scan must drain its work stack");

        // 3. Collect each root's White children as unreachable garbage. This phase
        //    keeps working in HeapId-space because `entry.free()` pushes to the
        //    free list, which is keyed by id.
        let freed = self.heap.collect_white(roots);

        // After this pass no Purple entries remain in the heap; zero the
        // counter so the next `dec_ref` event re-seeds from a clean baseline,
        // and reset the alloc-count gate so the next interval starts now.
        self.heap.purple_count = 0;
        self.heap.allocations_since_gc.set(0);
        freed
    }

    /// `MarkGray` (iterative): paint `s` and its transitive children Gray,
    /// decrementing each child's refcount once per traversal edge.
    ///
    /// After this completes for every root, every Gray entry's refcount equals
    /// the count of *external* references into it (refs originating outside
    /// the candidate subgraph). `Scan` uses that property to decide
    /// alive/condemned.
    fn mark_gray(&mut self, mut ptr: HeapPtr<'a, D>, work_stack: &mut Vec<HeapPtr<'a, D>>) {
        ptr.entry(self).color.set(CcColor::Gray);
        loop {
            ptr.data(self).for_each_child_id(|child_id| {
                let child_ptr = self.read_ptr(child_id);
                let entry = child_ptr.entry(self);
                debug_assert!(entry.refcount.get() > 0, "mark_gray: refcount underflow");
                entry.refcount.update(|r| r - 1);
                if entry.color.replace(CcColor::Gray) == CcColor::Gray {
                    // Already marked via another edge, don't push again
                    return;
                }
                work_stack.push(child_ptr);
            });
            let Some(next_ptr) = work_stack.pop() else {
                break;
            };
            ptr = next_ptr;
        }
    }

    /// `Scan` (iterative): each Gray entry is either resurrected via
    /// `ScanBlack` (external reference exists — refcount > 0 or active
    /// `HeapRead` reader) or painted White and its Gray children recursed.
    fn scan(&mut self, work_stack: &mut Vec<HeapPtr<'a, D>>) {
        let mut black_work_stack: Vec<HeapPtr<'a, D>> = Vec::new();
        while let Some(ptr) = work_stack.pop() {
            let entry = ptr.entry(self);
            if entry.color.get() != CcColor::Gray {
                // Already processed via another edge
                continue;
            }
            if entry.refcount.get() == 0 && entry.readers.get() == 0 {
                entry.color.set(CcColor::White);
                ptr.data(self).for_each_child_id(|child_id| {
                    let child_ptr = self.read_ptr(child_id);
                    let entry = child_ptr.entry(self);
                    if entry.color.get() != CcColor::Gray {
                        // Already processed via another edge
                        return;
                    }
                    work_stack.push(child_ptr);
                });
            } else {
                // External reference exists (either a refcount we couldn't
                // account for inside the candidate set, or a live `HeapRead`
                // pointing into the entry). Resurrect this entry and its
                // transitive Gray children back to Black via `mark_black`.
                self.mark_black(ptr, &mut black_work_stack);
                debug_assert!(black_work_stack.is_empty());
            }
        }
    }

    /// `ScanBlack` (iterative): resurrect a subtree by re-incrementing
    /// children's refcounts that `MarkGray` previously decremented, restoring
    /// the heap to the state it would have had if no cycle was suspected.
    ///
    /// Children's refcounts are incremented once per traversal edge — even if
    /// the child is already Black — so multi-edge graphs (a child reachable
    /// from two parents in the resurrected subtree) balance the matching
    /// per-edge decrements `MarkGray` performed. Recursion only descends into
    /// non-Black children so each entry is processed at most once.
    fn mark_black(&mut self, mut ptr: HeapPtr<'a, D>, work_stack: &mut Vec<HeapPtr<'a, D>>) {
        ptr.entry(self).color.set(CcColor::Black);
        loop {
            ptr.data(self).for_each_child_id(|child_id| {
                let child_ptr = self.read_ptr(child_id);
                let entry = child_ptr.entry(self);
                entry.refcount.update(|r| r + 1);
                if entry.color.replace(CcColor::Black) == CcColor::Black {
                    // Already marked via another edge
                    return;
                }
                work_stack.push(child_ptr);
            });
            let Some(next_ptr) = work_stack.pop() else {
                break;
            };
            ptr = next_ptr;
        }
    }

    /// Returns the live heap entries unreachable from `roots`, the payload type's
    /// [`initial_entries`](HeapObject::initial_entries) included.
    ///
    /// Reference-counting tests use this to prove a test leaked nothing: an entry
    /// that is alive but reachable from no named variable is a missed `drop_with`.
    /// The walk is transitive, so an object owned by another object — a class's
    /// `__annotations__`, a nested list, an instance attribute — is accounted
    /// for by its owner and need not be bound to a name by the test itself.
    ///
    /// Unlike the cycle collector's [`mark_gray`](Self::mark_gray) this touches
    /// no colors or refcounts, so it cannot perturb the state under test.
    #[cfg(feature = "ref-count-return")]
    #[must_use]
    pub fn unreachable_entries(&self, roots: impl IntoIterator<Item = HeapId>) -> Vec<HeapId> {
        let mut seen: HashSet<HeapId> = HashSet::new();
        let mut work_stack: Vec<HeapId> = Vec::new();
        for root in roots {
            if seen.insert(root) {
                work_stack.push(root);
            }
        }

        // A root's subtree is live by construction, but `try_entry` keeps the
        // walk total in case a test observes the heap mid-teardown.
        while let Some(id) = work_stack.pop() {
            let ptr = self.read_ptr(id);
            if ptr.try_entry(self).is_none() {
                continue;
            }
            ptr.data(self).for_each_child_id(|child_id| {
                if seen.insert(child_id) {
                    work_stack.push(child_id);
                }
            });
        }

        self.entries
            .iter()
            .filter(|(id, _)| !seen.contains(id))
            .map(|(id, _)| id)
            .collect()
    }
}

/// Cycle-collector unit tests.
///
/// These live beside the collector (rather than under `tests/`) because they
/// need to manipulate `Heap` state directly — building a cycle without a VM,
/// peeking at `purple_count`, and rooting an entry only via a Rust local
/// binding. They run over a minimal payload type so they test the collector,
/// not a language's object model.
///
/// In particular, [`cstack_only_cycle_survives_collection`] validates the
/// central correctness property of trial deletion: a heap entry referenced
/// *only* from the Rust C stack survives a cycle collection because its
/// non-zero refcount is itself proof of liveness. That behavior was previously
/// a known soundness gap of the explicit-roots mark–sweep collector.
#[cfg(test)]
mod tests {
    use monty_types::ResourceTracker;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::{Heap, HeapObjectRead};

    /// A payload with one container kind: a list of owned ids.
    #[derive(Debug, Serialize, Deserialize)]
    enum TestData {
        List(Vec<HeapId>),
    }

    /// Typed read handle for [`TestData`].
    enum TestRead<'a> {
        List(HeapObjectRead<'a, Vec<HeapId>>),
    }

    /// No singletons and no weak indexes.
    #[derive(Debug, Default, Serialize, Deserialize)]
    struct NoExtra {}

    impl HeapObject for TestData {
        type Extra = NoExtra;
        type Ref = HeapId;
        type Read<'a>
            = TestRead<'a>
        where
            Self: 'a;

        fn make_ref(id: HeapId) -> HeapId {
            id
        }

        fn read<'a>(ptr: HeapPtr<'a, Self>, id: HeapId, reader: &HeapReader<'a, Self>) -> TestRead<'a> {
            match ptr.data(reader) {
                Self::List(items) => TestRead::List(ptr.project(id, reader, items)),
            }
        }

        fn is_gc_tracked(&self) -> bool {
            true
        }

        fn for_each_child_id(&self, mut on_child: impl FnMut(HeapId)) {
            let Self::List(items) = self;
            for id in items {
                on_child(*id);
            }
        }

        fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
            let Self::List(items) = self;
            stack.append(items);
        }

        fn on_free(&mut self, _: &mut NoExtra, _: HeapId) {}

        fn on_restore(&mut self, _: &mut NoExtra, _: HeapId) {}
    }

    type TestHeap = Heap<TestData>;

    /// Appends `refs` to the list at `target`, counting each as a new reference.
    fn push_refs(heap: &mut TestHeap, target: HeapId, refs: &[HeapId]) {
        let TestData::List(items) = heap.get_mut(target);
        items.extend_from_slice(refs);
        for r in refs {
            heap.inc_ref(*r);
        }
    }

    /// Allocates a self-referencing one-element list and returns its id.
    ///
    /// The list's items become `[id]` and its refcount is bumped to 2 to
    /// reflect both the caller's ref and the new self-reference.
    fn alloc_self_cycle(heap: &mut TestHeap) -> HeapId {
        let id = heap.allocate(TestData::List(vec![]));
        push_refs(heap, id, &[id]);
        id
    }

    /// Allocates a two-element cycle where one direction has multiplicity 3:
    /// `P → [A, A, A]` and `A → [P]`. Returns `(p_id, a_id)`.
    ///
    /// Final refcounts: `P.rc = 2` (alloc + one edge from A), `A.rc = 4`
    /// (alloc + three edges from P). The caller "owns" one of each — both
    /// can be dropped via `dec_ref` to isolate the cycle.
    ///
    /// Exercises the duplicate-edges-within-one-element shape that
    /// `mark_gray`/`mark_black` must handle correctly: each edge from P
    /// to A is one independent decrement (or increment), but the work
    /// stack must not grow with edge multiplicity — otherwise the outer
    /// pop processes A's children multiple times and over-counts.
    fn alloc_dup_child_cycle(heap: &mut TestHeap) -> (HeapId, HeapId) {
        let p_id = heap.allocate(TestData::List(vec![]));
        let a_id = heap.allocate(TestData::List(vec![]));
        push_refs(heap, p_id, &[a_id, a_id, a_id]);
        push_refs(heap, a_id, &[p_id]);
        (p_id, a_id)
    }

    #[test]
    fn cstack_only_cycle_survives_collection() {
        let mut heap = TestHeap::new(16, ResourceTracker::default());
        let id = alloc_self_cycle(&mut heap);

        // Simulate a Rust-side local reference by bumping the refcount one
        // extra time. Then `dec_ref` it back down to 2 — that dec_ref is what
        // enrolls the entry as a Purple candidate, mimicking exactly the
        // situation under the old GC where the local binding wasn't published
        // in any explicit root set.
        heap.inc_ref(id); // rc = 3
        heap.dec_ref(id); // rc = 2, flagged Purple
        assert_eq!(heap.purple_count(), 1);

        // Cycle collection must not free the entry: the local "C-stack" ref
        // contributes one of its two surviving refcount units, so trial
        // deletion sees rc > 0 after MarkGray and resurrects the subtree.
        heap.collect_cycles();
        assert_eq!(heap.purple_count(), 0);
        assert!(heap.is_live(id), "C-stack-rooted cycle was freed");

        // Drop the simulated Rust local. Now the cycle is genuinely isolated
        // (rc 1 = self-pointer only). The next collection must reclaim it.
        heap.dec_ref(id); // rc = 1, re-flagged Purple
        assert_eq!(heap.purple_count(), 1);
        heap.collect_cycles();
        assert_eq!(heap.purple_count(), 0);
        assert!(!heap.is_live(id), "isolated cycle should have been freed");
    }

    #[test]
    fn heap_read_rooted_cycle_survives_collection() {
        let mut heap = TestHeap::new(16, ResourceTracker::default());
        let id = alloc_self_cycle(&mut heap);

        // Bump `readers` manually to mimic a live `HeapRead` pointing into
        // the entry. The borrow checker prevents holding a real `HeapRead`
        // across `collect_cycles` (which requires `&mut Heap`), so we
        // splice the same counter that `HeapRead::Drop` decrements.
        let readers_before = heap.entries.get(id).readers.get();
        heap.entries.get(id).readers.set(readers_before + 1);

        // Drive the entry into Purple via dec_ref: rc 2 → 1. Without the
        // `readers > 0` special-case in `Scan`, the resulting cycle would
        // be condemned to White and freed.
        heap.dec_ref(id); // rc = 1, flagged Purple
        assert_eq!(heap.purple_count(), 1);

        heap.collect_cycles();
        assert!(
            heap.is_live(id),
            "entry with active HeapRead reader was freed by collect_cycles"
        );

        // Restore the simulated reader so `Heap::drop` can clean up
        // without tripping the `dec_ref` active-readers assertion.
        heap.entries.get(id).readers.set(readers_before);
        // The entry is leaked here on purpose (rc = 1 from the self-ref,
        // no external root remains, but the collector ran already and the
        // color is Black — the next dec_ref would try to recurse into the
        // self-pointer after freeing the entry). `Heap::drop` walks every
        // slot and tears them down regardless of refcount, so leaking
        // here is safe for the duration of the test.
    }

    /// Regression test for a soundness gap in [`Heap::dec_ref`]'s trial-deletion
    /// candidate-enrollment path.
    ///
    /// The surviving-refcount branch once called `heap_entry.data.0.get_mut()` to
    /// check `is_gc_tracked`, even though that only needs `&self`. That
    /// `UnsafeCell::get_mut()` is a fresh `&mut D` (Unique) retag of the same
    /// allocation the live `HeapRead` is already pointing into via a
    /// `SharedReadWrite` raw pointer, so it invalidates the prior pointer under
    /// Stacked / Tree Borrows.
    ///
    /// Pure `cargo test` cannot observe this — the pointer arithmetic still
    /// reads valid bytes — but `cargo +nightly miri test` flags the access.
    /// The branch is reachable from normal interpreter code (e.g. `list.remove`
    /// on a self-referential list holds a `HeapObjectRead<List>`, clones the
    /// matching element, and the deferred drop of that clone calls
    /// `dec_ref` on the same `HeapId` while the list reader is still live).
    #[test]
    fn dec_ref_must_not_invalidate_live_heap_read() {
        let mut heap = TestHeap::new(16, ResourceTracker::default());
        let id = heap.allocate(TestData::List(vec![]));
        // Bump refcount so `dec_ref` enters the non-freeing branch where
        // the offending `data.0.get_mut()` lived. The payload is GC-tracked,
        // so `is_gc_tracked` returns true and the branch is fully exercised.
        heap.inc_ref(id);

        HeapReader::with(&mut heap, &mut (), |heap, ()| {
            let TestRead::List(list) = heap.read(id);
            // Holding `list` does NOT borrow the heap — only `list.get(heap)`
            // does. That is what lets the borrow checker accept this
            // sequence, while the underlying raw-pointer aliasing is
            // nevertheless violated by the next call.
            heap.dec_ref(id);
            // Read through the now-invalidated `SharedReadWrite` raw pointer.
            // Miri's aliasing model fails here.
            let _ = list.get(heap).as_slice().len();
        });
    }

    #[test]
    fn isolated_simple_cycle_is_collected() {
        // Sanity check: a self-reference cycle with no external rooting
        // gets collected on the next `collect_cycles` call.
        let mut heap = TestHeap::new(16, ResourceTracker::default());
        let id = alloc_self_cycle(&mut heap);
        // After alloc_self_cycle: rc = 2 (allocate's 1 + self-ref's 1).
        // Drop the caller's reference. rc 2 → 1, marks Purple.
        heap.dec_ref(id);
        assert_eq!(heap.purple_count(), 1);
        heap.collect_cycles();
        assert!(!heap.is_live(id));
        assert_eq!(heap.purple_count(), 0);
    }

    #[test]
    fn pending_purple_cycle_round_trips_through_serde() {
        // A snapshot can be taken between any two bytecode instructions, so
        // entries flagged Purple by `dec_ref` but not yet visited by the
        // collector must survive serde round-trips. Otherwise a cycle that
        // becomes garbage just before snapshot would leak permanently after
        // restore (the post-restore VM would never re-touch it).
        let mut heap = TestHeap::new(16, ResourceTracker::default());
        let id = alloc_self_cycle(&mut heap);
        // Drop the caller's external ref so the entry is genuinely
        // unreachable except via its self-pointer. dec_ref flags Purple.
        heap.dec_ref(id); // rc 2 → 1
        assert_eq!(heap.purple_count(), 1);
        assert_eq!(heap.entries.get(id).color.get(), CcColor::Purple);

        // Round-trip through the dump codec.
        let bytes = minicbor_serde::to_vec(&heap).expect("serialize");
        let mut restored: TestHeap = minicbor_serde::from_slice(&bytes).expect("deserialize");

        // `purple_count` and the per-entry color must round-trip.
        assert_eq!(restored.purple_count(), 1);
        assert_eq!(restored.entries.get(id).color.get(), CcColor::Purple);

        // Run the collector on the restored heap; the cycle is unreachable
        // and must be reclaimed.
        restored.collect_cycles();
        assert!(!restored.is_live(id));
        assert_eq!(restored.purple_count(), 0);
    }

    #[test]
    fn isolated_cycle_with_duplicate_child_refs_is_collected() {
        // Regression: an unreachable cycle where one element references its
        // sibling multiple times within its child list. The mark phase must
        // (a) decrement the sibling's refcount once per edge, and (b) push
        // the sibling onto the work stack at most once, even when many
        // edges from the same parent target it.
        //
        // Without (b), the outer pop walks the sibling's children once per
        // duplicate edge, over-decrementing the *sibling's* children's
        // refcounts. In debug builds this trips `mark_gray`'s
        // refcount-underflow `debug_assert`; in release it wraps the
        // refcount to `usize::MAX` and `scan` resurrects the cycle —
        // either way, the cycle is not collected.
        let mut heap = TestHeap::new(16, ResourceTracker::default());
        let (p_id, a_id) = alloc_dup_child_cycle(&mut heap);
        // After construction: P.rc = 2, A.rc = 4.

        // Drop the caller's refs; the cycle is now genuinely unreachable.
        heap.dec_ref(p_id); // P.rc 2 → 1, flagged Purple
        heap.dec_ref(a_id); // A.rc 4 → 3, flagged Purple
        assert_eq!(heap.purple_count(), 2);

        heap.collect_cycles();

        assert!(!heap.is_live(p_id), "P should be collected");
        assert!(!heap.is_live(a_id), "A should be collected");
        assert_eq!(heap.purple_count(), 0);
    }

    #[test]
    fn cstack_rooted_cycle_with_duplicate_child_refs_collects_after_pin_dropped() {
        // Regression: when `scan` resurrects a Purple cycle via `mark_black`,
        // the per-edge refcount *increment* must mirror `mark_gray`'s
        // per-edge decrement. Duplicate edges from one element to its
        // sibling must not over-increment via repeated outer pops of the
        // sibling.
        //
        // We pin P externally (extra inc_ref) so `scan` is forced to take
        // the resurrect path through `mark_black`. After collection the
        // refcounts must be exactly the pre-collection values — verified by
        // dropping the pin and confirming the next `collect_cycles` reclaims
        // the cycle. If `mark_black` over-incremented during resurrection,
        // dropping the pin leaves the refcounts artificially high and the
        // cycle leaks.
        let mut heap = TestHeap::new(16, ResourceTracker::default());
        let (p_id, a_id) = alloc_dup_child_cycle(&mut heap);
        // After construction: P.rc = 2, A.rc = 4.

        // Pin P externally. After the inc_ref + dec_ref pair, P is Purple
        // but its refcount still includes one unit not accounted for by
        // any in-cycle edge — `scan` must resurrect via `mark_black`.
        heap.inc_ref(p_id); // P.rc = 3 (alloc + from A + pin)
        heap.dec_ref(p_id); // P.rc = 2, flagged Purple
        heap.dec_ref(a_id); // A.rc = 3, flagged Purple

        heap.collect_cycles();

        // First pass: cycle survives because of P's external pin.
        assert!(heap.is_live(p_id), "P should survive (external pin)");
        assert!(heap.is_live(a_id), "A should survive (reachable via P)");

        // Drop the pin. If `mark_black` correctly restored refcounts during
        // resurrection, the cycle is now isolated and the next collection
        // reclaims it. Over-incrementing in `mark_black` leaves P's
        // refcount artificially high so `scan` resurrects it again.
        heap.dec_ref(p_id);
        heap.collect_cycles();

        assert!(!heap.is_live(p_id), "P should be collected after pin dropped");
        assert!(!heap.is_live(a_id), "A should be collected after pin dropped");
    }
}
