//! Reference-counted, cycle-collected paged arena shared by Monty's sandbox interpreters.
//!
//! A [`Heap`] stores one payload per entry. The payload type implements
//! [`HeapObject`] (how to trace, release and read it), so the arena, the
//! reference counting, the Bacon–Rajan trial-deletion collector, the
//! [`HeapReader`] access discipline and the dump serialization are written once
//! and instantiated per language: the Python interpreter in `monty` today, the
//! JavaScript engine next. Nothing here knows what a Python list or a
//! JavaScript object is.
//!
//! This crate contains the `unsafe` code that the interpreters' heap access
//! relies on; read the safety comments in each module before changing anything.

#![expect(
    unsafe_code,
    reason = "Paged arena hands out typed, aliasable views into UnsafeCell entries"
)]

use std::fmt;

use serde::{Serialize, de::DeserializeOwned};

mod access;
mod arena;
mod cleanup;
mod collect;
mod free_list;
mod stable_heap;

pub use access::{
    BorrowedHeapRead, BorrowedHeapReadMut, HeapAllocation, HeapObjectRead, HeapPtr, HeapRead, HeapReader,
    cast_as_member_ref_mut_type_hinted, cast_as_member_ref_type_hinted,
};
pub use arena::{CcColor, Heap, HeapEntry};
pub use cleanup::{DropGuard, DropWithContext};
pub use stable_heap::StableHeap;

/// Unique identifier for values stored inside the heap arena.
///
/// The ID does not encode ownership. Local IDs should normally be borrowed or
/// wrapped immediately in the language's heap-reference value; owned fields
/// must document and release their reference through the language's cleanup
/// traits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct HeapId(usize);

impl HeapId {
    /// Creates a `HeapId` from a raw index.
    #[inline]
    #[must_use]
    pub const fn from_index(index: usize) -> Self {
        Self(index)
    }

    /// Returns the raw index value.
    #[inline]
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

/// The payload a [`Heap`] stores: one value per entry, usually an enum over a
/// language's object kinds.
///
/// The heap calls back into the payload for everything language-specific: which
/// entries can form cycles, which ids an entry owns (traced by the collector and
/// released when the entry is freed), how to turn an entry into a typed read
/// handle, and how to keep per-heap singletons and weak indexes in sync. The
/// tracing and release callbacks MUST report exactly the same ids: reporting
/// fewer leaks, reporting more is a use-after-free.
pub trait HeapObject: Sized + fmt::Debug + Serialize + DeserializeOwned {
    /// Per-heap state beside the entries: singleton ids and weak indexes.
    ///
    /// A struct, serialized flattened into the heap's own map, so persistent
    /// fields keep their names in a dump and `#[serde(skip)]` fields are
    /// rebuilt through [`on_restore`](Self::on_restore).
    type Extra: Default + fmt::Debug + Serialize + DeserializeOwned;

    /// The language's owned heap-reference value, e.g. `Value::Ref`.
    type Ref;

    /// The typed read handle [`HeapReader::read`] returns, one variant per payload kind.
    type Read<'a>
    where
        Self: 'a;

    /// Entries present in every new heap, in id order from zero, each owning
    /// one permanent reference; singletons such as Python's empty tuple.
    #[must_use]
    fn initial_entries() -> Vec<Self> {
        Vec::new()
    }

    /// Wraps an owned id in the language's reference value.
    fn make_ref(id: HeapId) -> Self::Ref;

    /// Projects the entry at `ptr` into its typed read handle, through
    /// [`HeapPtr::project`] or [`HeapPtr::project_boxed`].
    fn read<'a>(ptr: HeapPtr<'a, Self>, id: HeapId, reader: &HeapReader<'a, Self>) -> Self::Read<'a>;

    /// Whether this entry can take part in a reference cycle. Leaf payloads
    /// (strings, numbers) return false and are never collector candidates.
    fn is_gc_tracked(&self) -> bool;

    /// Calls `on_child` with every id this entry owns, once per owned reference.
    fn for_each_child_id(&self, on_child: impl FnMut(HeapId));

    /// Pushes every id this entry owns onto the iterative release stack.
    fn dec_ref_ids(&mut self, stack: &mut Vec<HeapId>);

    /// Runs when the entry at `id` is freed, before its slot can be reused, so a
    /// weak index pointing at it can be cleared.
    fn on_free(&mut self, extra: &mut Self::Extra, id: HeapId);

    /// Runs for every live entry when a heap is restored from a dump, so weak
    /// indexes can be rebuilt.
    fn on_restore(&mut self, extra: &mut Self::Extra, id: HeapId);
}

/// A concrete payload type stored inside `D`: converts into its `D` variant and
/// back out of `D`'s typed read handle.
///
/// Implemented per variant by the language's payload registry, which lets
/// [`HeapReader::allocate_as`] and [`HeapReader::read_as`] work with the concrete
/// type instead of the enum.
pub trait Payload<D: HeapObject>: Sized {
    /// The language's typed handle for this payload: its wrapper around
    /// [`HeapObjectRead`], which is where the payload's methods live.
    type Handle<'a>
    where
        Self: 'a,
        D: 'a;

    /// Wraps this payload in its `D` variant.
    fn into_data(self) -> D;

    /// Extracts this payload's typed handle from a dynamic read, or hands the
    /// read back when the entry holds a different payload.
    fn try_from_read<'a>(read: D::Read<'a>) -> Result<Self::Handle<'a>, D::Read<'a>>
    where
        Self: 'a,
        D: 'a;
}

/// Gives cleanup machinery access to an owned heap.
///
/// Implemented by [`Heap`], [`HeapReader`] and whatever a language wraps them
/// in (its VM, an encoder), so one `drop_with` call works with any of them.
pub trait ContainsHeap {
    /// The payload type of the heap this value holds.
    type Data: HeapObject;

    /// Borrows the heap.
    fn heap(&self) -> &Heap<Self::Data>;

    /// Mutably borrows the heap.
    fn heap_mut(&mut self) -> &mut Heap<Self::Data>;
}

impl<D: HeapObject> ContainsHeap for Heap<D> {
    type Data = D;

    fn heap(&self) -> &Self {
        self
    }

    #[inline]
    fn heap_mut(&mut self) -> &mut Self {
        self
    }
}
