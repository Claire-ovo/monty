# JavaScript Sandbox

Monty runs JavaScript as well as Python.
A JavaScript pool spawns the `monty-js` worker, which runs Monty's own JavaScript engine behind the same protocol
as the Python worker, so sessions, host functions, printed output, resource limits and snapshots work the same way.
The engine is young: the subset it runs and every divergence from Node are listed on
[its limitations page](limitations/javascript.md).

## Running JavaScript

Pick the language when creating the pool.
Every session of that pool feeds JavaScript, and its default script name is `main.js`.

=== "Python"

    ```python
    from pydantic_monty import Monty

    with Monty(language='javascript') as pool:
        with pool.checkout() as session:
            session.feed_run('const squares = [1, 2, 3].map((n) => n ** 2);')
            total = session.feed_run('squares.reduce((a, b) => a + b, 0)')
            print(total)
            #> 14
    ```

=== "TypeScript"

    ```ts
    import { Monty } from '@pydantic/monty'

    await using pool = await Monty.create({ language: 'javascript' })
    await using session = await pool.checkout()
    await session.feedRun('const squares = [1, 2, 3].map((n) => n ** 2);')
    const total = await session.feedRun('squares.reduce((a, b) => a + b, 0)')
    console.log(total)
    ```

In Python the worker comes from the `pydantic-monty-js-runtime` wheel, installed by the `js` extra
(`pip install "pydantic-monty[js]"`); the npm platform packages ship `monty-js` next to `monty`.
As with the Python worker, `binary_path` / `binaryPath` or the `MONTY_JS_BIN` environment variable name the binary
explicitly.

## Host functions and values

An undeclared name that the snippet calls is a host function: the feed suspends with a `FunctionCall`, and the
host's return value becomes the call's result, exactly as for Python (see [Host Functions](host-functions.md)).
A host function that returns a future (an `async def` in Python, a promise in TypeScript) gives the snippet a
promise, so `await fetchData()` works in both directions.
An undeclared name that is only read suspends with a `NameLookup`.

=== "Python"

    ```python
    from pydantic_monty import Monty

    with Monty(language='javascript') as pool:
        with pool.checkout() as session:
            result = session.feed_run(
                'const rows = await load("users"); rows.map((r) => r.name).join(", ")',
                external_lookup={'load': lambda table: [{'name': 'ada'}, {'name': 'lin'}]},
            )
            print(result)
            #> ada, lin
    ```

=== "TypeScript"

    ```ts
    import { Monty } from '@pydantic/monty'

    await using pool = await Monty.create({ language: 'javascript' })
    await using session = await pool.checkout()
    const result = await session.feedRun('const rows = await load("users"); rows.map((r) => r.name).join(", ")', {
      externalLookup: { load: async (_table: string) => [{ name: 'ada' }, { name: 'lin' }] },
    })
    console.log(result)
    ```

Values cross the boundary as the same graph the Python worker uses: arrays become lists, plain objects and `Map`s
become dicts (`Map`s again in the JavaScript binding), `Set`s become sets, `Date`s become aware `datetime`s, and
an uncaught error ends the feed as the exception of the nearest Python class (`TypeError`, `NameError` for a
`ReferenceError`, `Exception` for a plain `Error`).
The full mapping is in [the limitations page](limitations/javascript.md#host-values).

## Limits, clocks and snapshots

[Resource limits](resource-limits.md) apply unchanged: memory, feed and turn durations, recursion depth and
suspensions.
A `Date` reads the session's `OsPolicy` clock and zone, and `Math.random` its entropy policy, so a fixed clock or
seed makes a JavaScript session reproducible in the same way as a Python one.
[Snapshots](snapshots.md) capture idle and suspended JavaScript sessions too, though a JavaScript dump cannot be
loaded into a Python worker or vice versa.
Type checking is not available for JavaScript yet.
