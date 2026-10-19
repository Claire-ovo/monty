# monty-heap

The reference-counted, cycle-collected paged arena shared by Monty's sandbox interpreters.

A `Heap<D>` stores one payload per entry. The payload type implements `HeapObject`, which tells the heap everything
language-specific: which entries can take part in a cycle, which ids an entry owns (traced by the collector and released
when the entry is freed), how to project an entry into a typed read handle, and how to keep per-heap singletons and weak
indexes (`HeapObject::Extra`) in sync. Everything else is written once:

- `StableHeap<T>`: paged storage whose entries never move, so a reference into an entry stays valid while new entries
  are allocated through `&self`, with a free list that reuses freed slots.
- `Heap<D>`: reference counting (`inc_ref` / `dec_ref`, iterative so deep structures free without recursion) and
  Bacon–Rajan trial-deletion cycle collection seeded from `dec_ref`, so no root enumeration is needed; a
  `ResourceTracker` schedules collections and enforces limits.
- `HeapReader<'a, D>`: the branded, scoped access discipline. `HeapReader::with` is the only constructor, and its
  `for<'a>` closure makes the brand invariant, so a `HeapRead` handle cannot outlive or escape the reader it came from.
  Every handle holds a reader count on its entry, which `dec_ref` and the collector treat as a live reference.
- `HeapRead`, `HeapObjectRead`, `BorrowedHeapRead`, `BorrowedHeapReadMut`: the typed handles, built by
  `HeapPtr::project` / `project_boxed` from a field reference the payload type matched out of its own enum. A language
  wraps them in newtypes of its own, named by `Payload::Handle`, so its object methods can be inherent impls.
- `DropWithContext`, `DropGuard`, `defer_drop!` / `defer_drop_mut!`: explicit release of owned references on every
  code path.
- Dump serialization: entries, collector colours and the payload type's `Extra` (flattened into the heap's own map),
  with weak indexes rebuilt through `HeapObject::on_restore`.

The Python interpreter (`monty`) instantiates it with `HeapData`; the JavaScript engine is the second user.

## Features

- `ref-count-return`: per-entry reference counts, live-entry counts and the leak detector
  (`HeapReader::unreachable_entries`) for the interpreters' reference-counting tests.
- `memory-model-checks`: collect on every GC-tracked allocation and tear the heap down on drop, so a payload type's own
  debug bookkeeping can catch leaked references.
- `test-hooks`: `disable_gc` / `enable_gc`, for tests that time a single collection.

## Safety

This crate holds the `unsafe` code the interpreters' heap access rests on: pointer arithmetic into entries,
`UnsafeCell` access and the reader-count invariants. The repository's `CLAUDE.md` rules for `crates/monty/src/heap/`
apply here exactly: every `unsafe` block states the invariant it relies on, and changes need explicit approval and
careful review. `make miri` runs the inline tests under Miri.
