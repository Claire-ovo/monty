//! Python-side cleanup traits over `monty_heap`'s: [`HeapItem`] for payload
//! types that own references, and the aliases that pin the shared traits to the
//! Python heap so bounds throughout the interpreter stay short.

pub(crate) use monty_heap::{DropGuard, DropWithContext};

use crate::heap::{HeapData, HeapId};

/// Collects owned heap references when a heap entry is released.
///
/// `HeapData` variants owning heap references implement this trait (or are
/// handled inline in `HeapData::dec_ref_ids`) so reference-count cleanup can
/// walk children iteratively without requiring the Python-level `PyTrait` API;
/// leaf variants with no children fall through the dispatch's `_ => {}` arm.
pub(crate) trait HeapItem {
    /// Pushes every owned heap reference onto the iterative cleanup stack.
    ///
    /// Owned `HeapId` fields must be documented as owned and pushed exactly
    /// once. With `memory-model-checks`, delegation also marks contained
    /// `Value`s as `Dereferenced`.
    fn py_dec_ref_ids(&mut self, stack: &mut Vec<HeapId>);
}

/// Gives cleanup machinery access to an owned Python heap.
///
/// `monty_heap::ContainsHeap` pinned to [`HeapData`]: `Heap`, `HeapReader`, the
/// `VM` and the json `Encoder` all satisfy it, and a bound `C: ContainsHeap` lets
/// a value release its references through whichever the caller holds.
pub(crate) trait ContainsHeap: monty_heap::ContainsHeap<Data = HeapData> {}

impl<T: monty_heap::ContainsHeap<Data = HeapData>> ContainsHeap for T {}

/// A concrete payload type stored in the Python heap: `monty_heap::Payload`
/// pinned to [`HeapData`], implemented for every registered variant.
pub(crate) trait HeapPayload: monty_heap::Payload<HeapData> {}

impl<T: monty_heap::Payload<HeapData>> HeapPayload for T {}
