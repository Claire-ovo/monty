//! Reference-counted, cycle-collected paged arena shared by Monty's sandbox interpreters.
//!
//! The crate owns the storage layer every heap is built on: [`HeapId`], the
//! address-stable [`StableHeap`] pages and their free list. The Python
//! interpreter in `monty` instantiates it today; the JavaScript engine is the
//! second user. Nothing here knows what a payload is, so a language's object
//! model stays in its own crate.
//!
//! This crate contains the `unsafe` code that the interpreters' heap access
//! relies on; see the safety comments in each module before changing anything.

#![expect(
    unsafe_code,
    reason = "Paged arena hands out typed, aliasable views into UnsafeCell entries"
)]

mod free_list;
mod stable_heap;

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
