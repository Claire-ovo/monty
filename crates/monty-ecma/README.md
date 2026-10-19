# monty-ecma

The JavaScript engine of [Monty](https://github.com/pydantic/monty): a sandboxed interpreter that parses with
[oxc](https://oxc.rs), compiles to its own bytecode and runs it on a stack VM over `monty-heap`'s reference-counted,
cycle-collected arena. It is the engine behind the `monty-js` worker binary (`monty-js-runtime`), which serves it over
the same wire protocol as the Python `monty` worker, so `monty-pool` and the language bindings drive both alike.

Every snippet runs as strict-mode module code (sloppy-mode syntax is a `SyntaxError`), top-level `await` is allowed,
and a session keeps its `globalThis` properties and top-level `let`/`const`/`class` bindings between feeds.

## API

The surface mirrors the Python interpreter's REPL:

- `JsRepl::new(script_name, limits)` creates an idle session; `with_os_policy` sets the clock, zone, sleep and
  entropy policy that `Date`, timers and `Math.random` observe.
- `JsRepl::feed(code, inputs, print)` runs one snippet with named host values in scope, streaming `console.*` output
  to the `PrintWriter`, and returns a `JsProgress`: `Complete` with the completion value as a `MontyObject`,
  `Failed` with a `MontyException`, or `Suspended` when the host must act.
- `JsSuspension::kind()` names what the host must answer (a host function call, a name lookup, pending host futures
  or an OS call for the clock) and `resume_call` / `resume_name_lookup` / `resume_futures` / `abort` continue the feed.
- `dump` / `JsDump::load` snapshot an idle or suspended session as CBOR behind a `MONTYJS` header and `DUMP_VERSION`.

Values cross the boundary as `monty_types::MontyObject` graphs: arrays map to lists, plain objects and `Map`s to
dicts, `Set`s to sets, `Date`s to aware datetimes, and an uncaught error to the nearest `MontyException` class.

## Layout

- `parse.rs`, `resolve.rs` — oxc parsing and scope analysis (which bindings closures capture, TDZ checks).
- `compile/` — AST to bytecode: heap cells for captured bindings, fresh cells per `for (let …)` iteration,
  `finally` blocks inlined on every exit path.
- `bytecode/` — the opcode set and the code builder.
- `vm/` — the dispatch loop, object model, coercions and operators, calls, generators, promises and the job queue,
  host suspensions, and a port of Node's `util.inspect` for `console.log`.
- `builtins/` — every native as a `NativeFn` variant (a stable id, so function objects serialize without addresses):
  `Object`, `Function`, `Array`, `String`, `Number`, `Math`, `JSON`, `RegExp` (via `monty-regexp`), `Map`/`Set`,
  `Symbol`, the `Error` family, `Promise` and timers, `Date`, generators and `console`.
- `object_bridge.rs` — `MontyObject` import and export; `dump.rs` — session snapshots.

## Features

- `memory-model-checks`: collect on every GC-tracked allocation, to shake out missed releases.
- `ref-count-return`: per-entry reference counts and `JsRepl::leaked_entries`, the heap entries no root reaches after a
  feed; the differential harness asserts there are none for every case.

The engine is the second user of `monty-heap` and depends on `monty-types`, never on the Python interpreter.
`docs/limitations/javascript.md` in the repository records every divergence from Node.
