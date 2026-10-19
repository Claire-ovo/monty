# monty-regexp

The regular-expression engine of Monty's JavaScript sandbox, and the checksum-pinned Unicode 17 tables it and the
engine's `String` builtins use.

The crate has `#![forbid(unsafe_code)]` and no dependencies. Its API works on UTF-16 code units (`&[u16]`, or
Latin-1 `&[u8]` where a function takes `CodeUnits`) and plain `u32` code points; it never sees an engine string or a
heap object. The JavaScript engine compiles regular-expression literals through `compile`, runs them through
`execute_with_interrupt` (the interrupt closure is where the sandbox's time and backtracking budgets are checked), and
owns the `RegExp` objects, coercions and user callbacks itself.

| Module | Content |
| --- | --- |
| crate root | `compile`, `CompiledRegExp`, `RegExpFlags`, `Instruction`, `execute` and its interrupt and Latin-1 forms, `RegExpMatch`, the error types, `javascript_compile_error_message` |
| `unicode` | `is_id_start`, `is_id_continue`, `CodeUnits`, `CodeUnitBuf`, `StringError`, `MAX_STRING_LEN` |
| `unicode::case` | `convert_case_with_limit`, `regexp_canonicalize`, `regexp_canonicalize_range_pairs` |
| `unicode::normalize` | `NormalizationForm`, `normalize`, `normalize_with_limit`, `normalize_code_points` |
| `unicode::property` | the property sets of RegExp `\p{…}` escapes |

The `test-hooks` feature exposes the thread-local allocation-failure hooks of the case and normalization buffers to the
engine's `String` tests.

```sh
cargo test -p monty-regexp
```

Sources: [`src/`](src/README.md).

## Provenance

This crate is a vendored copy of `oxide-regexp`, the regular-expression crate of
[quickjs-oxide](https://github.com/pocket-nexus/quickjs-oxide), taken from commit
`109e165430feda1f0bfe6ddd587c5312ed7a43d9` (2026-10-19). The algorithms are unchanged; the copy differs in crate name,
manifest and documentation, in being formatted with this workspace's rustfmt settings, and in the imports, `#[expect]`
attributes and small rewrites its clippy configuration (pedantic) asked for. The generated tables under
`src/unicode/generated/` are byte-identical to upstream.
quickjs-oxide is MIT licensed; it is a Rust implementation of QuickJS's `libregexp` and `libunicode`, and the generated
Unicode tables under `src/unicode/generated/` derive from QuickJS 2026-06-04's `libunicode-table.h` and Unicode 17.0.0
UCD data. The QuickJS MIT notice and the Unicode License v3 notice are retained in [`LICENSES/`](LICENSES/).

Upstream is tracked by hand: to pick up a newer `regexp/`, diff it against this crate's `src/` and port the changes,
keeping the module layout so the diff stays readable.
