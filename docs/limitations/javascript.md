# JavaScript sandbox

The `monty-js` worker runs JavaScript on Monty's own engine (`monty-ecma`), behind the same protocol as the Python
worker.
Its reference is Node 25 running the same code as an ES module; this page records where the engine behaves
differently.
The engine is at an early stage, so most entries are features that are not implemented at all.

## Execution model

- Every feed is parsed as a strict-mode ES module: sloppy-mode-only syntax (`with`, legacy octal literals, duplicate
    parameter names, `delete identifier`) is a `SyntaxError`, `this` at the top level is `undefined`, and a function
    called without a receiver sees `this === undefined`.
- `import` and `export` declarations are a `SyntaxError` ("module declarations is not supported"); `import()`,
    `import.meta` and `require` do not exist.
- Top-level `var` and function declarations become `globalThis` properties, and top-level `let`, `const` and `class`
    bindings persist across feeds in a session-wide scope; a later feed may redeclare one, which replaces the binding
    instead of raising the `SyntaxError` a module would.
- The completion value of the last top-level expression statement is the feed's result, as a REPL would report it.
- Syntax errors carry oxc's wording (`Unexpected token`) rather than V8's, and the `SyntaxError` names the whole
    feed.
- An uncaught throw ends the feed with the thrown error's nearest builtin class (`Error`, `TypeError`, `RangeError`,
    `ReferenceError`, `SyntaxError`, `EvalError`, `URIError`, `AggregateError`) and its `message`; a thrown non-error
    value is reported as `Error` with the message `Uncaught <value>`.
- Stack traces (`Error.prototype.stack`) use V8's `    at name (file:line:column)` shape with the feed's script name
    as the file, and show at most ten frames.

## Language features not implemented

Each of these is a `SyntaxError` at compile time naming the feature (`generators is not supported`):

- Private fields and methods (`#x`), `accessor` properties, decorators
- `super()` inside an arrow function in a derived constructor
- A computed key before a rest element in an object pattern (`const { [k]: a, ...rest } = o`)
- Async functions, `await`, `for await` and async generators
- `BigInt` literals; the `BigInt` global exists so feature checks work, but calling it throws a `TypeError`
- Tagged templates
- `using` declarations
- TypeScript and JSX syntax

Generators (`function*`, `yield`, `yield*`, generator methods) work as in Node, except that
`Object.getPrototypeOf(function* () {}).constructor` (the `GeneratorFunction` constructor) does not exist, so a
generator function cannot be built from source text.

Within classes:

- A computed field key (`[expr] = value`) is evaluated when each instance is initialized rather than once when
    the class is defined.
- `super.x` reads a getter with the home object's prototype as its receiver rather than the current `this`;
    `super.x = v` is not supported.
- `Symbol.hasInstance` is not consulted by `instanceof`.

`eval` and `new Function` are not available (`Function()` throws an `EvalError`).

## Builtins

Available: `globalThis`, `undefined`, `NaN`, `Infinity`, `parseInt`, `parseFloat`, `isNaN`, `isFinite`, `console`
(`log`, `info`, `debug` to stdout; `error`, `warn` to stderr), `Object`, `Function.prototype` (`call`, `apply`,
`bind`, `toString`), `Array`, `String`, `Number`, `Boolean`, `Symbol` (with the well-known symbols), `Map`, `Set`,
`WeakMap`, `WeakSet`, `RegExp`, `Math`, `JSON` and the `Error` family.

Not available: `WeakRef`, `FinalizationRegistry`, `Promise`, `Date`, `Proxy`, `Reflect`,
`ArrayBuffer` and the typed arrays, `TextEncoder`/`TextDecoder`, `structuredClone`, `queueMicrotask`,
`setTimeout` and the other timers, `encodeURIComponent` and its relatives, `Intl`, `Atomics`, `globalThis.process`
and every Node module.

Differences within the available builtins:

- `Array.prototype` lacks `group` and the locale methods; `String.prototype` lacks `localeCompare` with locales
    (it compares code units), `substr`, `anchor` and the other HTML methods. `String.prototype.normalize` uses the
    Unicode 17 tables vendored with the regexp engine rather than the host's ICU.
- `RegExp` is the ECMAScript engine vendored from QuickJS (`monty-regexp`): all flags (`dgimsuvy`), named groups,
    lookbehind, Unicode property escapes and `v`-mode set operations work, and `String.prototype.match`,
    `matchAll`, `replace`, `replaceAll`, `search` and `split` dispatch through the `Symbol.match`/`matchAll`/
    `replace`/`search`/`split` protocol, so a custom matcher object works as it does in Node. Differences:
    an invalid pattern reports V8's `Invalid regular expression: /…/…: ` prefix followed by QuickJS's wording
    (`unexpected end` where V8 says `Unterminated character class`); `RegExp.escape`, `RegExp.prototype.compile`,
    the legacy statics (`RegExp.$1`, `RegExp.lastMatch`) and `Symbol.species` are absent, so the results of
    `split`/`matchAll` on a subclass are built with the builtin `RegExp`; a match that exhausts the backtracking
    stack throws `RangeError: Maximum call stack size exceeded` where V8 keeps going; and a pattern that runs past
    the feed's time limit ends the feed with the limit's error rather than returning.
- `WeakMap` and `WeakSet` hold their keys strongly: a key stays alive as long as the collection does, since the
    sandbox has no finalization. `console.log` prints them as Node does, `WeakMap { <items unknown> }`.
- A `Map` or `Set` keeps a tombstone for each deleted entry until it is cleared, so a collection with heavy
    insert/delete churn uses memory proportional to the insertions, within the memory limit.
- `Object.freeze` on an array moves its elements into named properties to make them read-only; the array keeps
    reporting `Array.isArray(a) === true` and `a.length`, but later `push` calls append to a separate element store.
- Arrays are always dense: assigning an index more than 1 048 576 past the end throws a `RangeError`
    ("Array index too sparse") instead of creating a sparse array, and `length` can grow by at most that much at once.
- `for…in` snapshots the enumerable keys before the first iteration, so keys added or deleted during the loop do not
    affect it.
- A `for…of` loop does not call the iterator's `return()` when the body throws (it does on `break` and `return`),
    and array destructuring does not call it when a `next()` call throws.
- The not-iterable error of a destructuring declaration names the initializer as V8's call printer renders it
    (`{}`, `null`, an identifier) and falls back to `(intermediate value)` for other expressions; V8 prints the full
    source text.
- `Symbol.toPrimitive` is consulted, but `Symbol.toStringTag`, `Symbol.hasInstance`, `Symbol.species`,
    `Symbol.isConcatSpreadable` and `Symbol.unscopables` have no effect.
- `Math.random()` draws from a session-local generator with a fixed seed, so a session's sequence is reproducible
    and identical across sessions.
- `Number.prototype.toString(radix)` prints at most 52 fractional digits for a non-integer; V8 prints the shortest
    representation that round-trips.
- `Function.prototype.toString` returns the function's source text for compiled functions and
    `function name() { [native code] }` for builtins; the `name` of a function defined by `defineProperty` or in a
    computed-key position is the empty string.
- `console.log` follows Node's `util.inspect` with its defaults (depth 2, 100 array items, 10 000 string
    characters, 80-column line breaking) and `%s %d %i %f %j %o %O %c %%` directives; colours, `Map`/`Set`/class
    instances (which do not exist), `Proxy` and `WeakMap` output are not reproduced, and a boxed `Symbol` prints as
    `[Symbol: Symbol(x)]`.
- `JSON.parse` reports V8's messages and positions; a quoted preview of the input shows the whole text up to 40
    characters, else a 20-character window around the error.
- `JSON.stringify` has no `BigInt` to reject and treats a boxed primitive as its primitive; the `indent` argument
    is honoured up to 10 characters.

## Host values

The sandbox exchanges values with the host as the same graph the Python worker uses, so the mapping is lossy in
both directions:

| JavaScript                                | Host value (`MontyObject`)                                         |
| ----------------------------------------- | ------------------------------------------------------------------ |
| `undefined`, `null`                       | `None`                                                             |
| integral number in the safe range, `-0`   | `int` (`-0` becomes `0`)                                           |
| other numbers                             | `float`                                                            |
| string                                    | `str` (a lone surrogate becomes U+FFFD)                            |
| array, `arguments`                        | `list`                                                             |
| plain object                              | `dict` with `str` keys, own enumerable properties, insertion order |
| `Error`                                   | exception of the nearest builtin class with its `message`          |
| boxed primitive                           | the primitive                                                      |
| function, symbol, iterator, anything else | `repr` string                                                      |

Inputs convert the other way: `list`, `tuple`, `set`, `frozenset` and named tuples become arrays, `dict` becomes a
plain object (keys converted with `ToPropertyKey`), `bytes` becomes an array of numbers, `int`s beyond the double
range lose precision, `datetime`/`date`/`time`/`timedelta`/`complex`/`Path` become their string representation,
exceptions become `Error` objects, and a shared sub-object is shared again in the sandbox.
A cycle in an exported value is cut with a `[...]` / `{...}` placeholder.

## Not yet available through the protocol

Host functions, `NameLookup` for undefined globals, `ResolveFutures`, OS calls (the clock, sleeps, the filesystem),
dumps and loads all end the turn with an error; type checking (`Configure.type_check`) is refused.
