# monty-heap

The reference-counted, cycle-collected paged arena shared by Monty's sandbox interpreters.

The Python interpreter (`monty`) instantiates it today; the JavaScript engine is the second user. The crate owns the
storage layer and the `unsafe` code the interpreters' heap access is built on:

- `HeapId`, the index of an entry; it carries no ownership information.
- `StableHeap<T>`, paged storage whose entries never move once allocated, so a reference into an entry stays valid
  while new entries are allocated through `&self`, with a free list that reuses freed slots.

The security review notes in the repository's `CLAUDE.md` apply to this crate exactly as they apply to
`crates/monty/src/heap/`: every `unsafe` block states the invariant it relies on, and changes need careful review.
