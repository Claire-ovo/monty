# JavaScript Sandbox

Monty runs JavaScript and TypeScript as well as Python.
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
A host function's return value is the call's result; one that returns a future (a coroutine under
[`AsyncMonty`][pydantic_monty.AsyncMonty], a promise in TypeScript) gives the snippet a promise instead, so
`await load("users")` works whether the host answers now or later.
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

Values cross the boundary as the same graph the Python worker uses, so arrays, plain objects, `Map`s, `Set`s,
`Date`s and errors arrive as the host types the Python sandbox would produce.
The mapping, and what it loses, is in [the limitations page](limitations/javascript.md#host-values).

## TypeScript

A pool created with `language='typescript'` runs the same worker on TypeScript source: types are erased before the
code runs, with no type checking.
Sessions default to the script name `main.ts`, and it is the script name's extension that selects the dialect, so a
JavaScript pool runs a `.ts` session too.
The engine accepts the erasable syntax Node's own type stripping accepts (annotations, interfaces, type aliases,
generics, `as`, `satisfies`, `!`, `declare`, type-only imports) and rejects, with Node's messages, the constructs that
would need emission: enums, namespaces with values, parameter properties, `import =`, `export =` and the
angle-bracket assertion.

=== "Python"

    ```python
    from pydantic_monty import Monty

    with Monty(language='typescript') as pool:
        with pool.checkout() as session:
            result = session.feed_run("""
    interface User { name: string; age?: number }
    const users: User[] = [{ name: 'ada', age: 36 }, { name: 'lin' }];
    users.map((user): string => `${user.name}:${user.age ?? 'unknown'}`) satisfies string[];
    """)
            print(result)
            #> ['ada:36', 'lin:unknown']
    ```

=== "TypeScript"

    ```ts
    import { Monty } from '@pydantic/monty'

    await using pool = await Monty.create({ language: 'typescript' })
    await using session = await pool.checkout()
    const result = await session.feedRun(`
    interface User { name: string; age?: number }
    const users: User[] = [{ name: 'ada', age: 36 }, { name: 'lin' }];
    users.map((user): string => user.name + ':' + (user.age ?? 'unknown')) satisfies string[];
    `)
    console.log(result)
    ```

## JSX

A session whose script name ends in `.tsx` or `.jsx` accepts JSX.
There is no React: an element evaluates eagerly to a plain tree the host can consume, `{ type, props, children }`,
where `children` is a flat array of nodes and strings (nullish and boolean children are dropped, numbers become
strings, nested arrays flatten).
A component is a function called with its props, `children` included, and its return value stands in for the
element; a fragment evaluates to its children array.
A component may be `async`: an element with a pending child is a promise of its node, so `await <App />` gives the
settled tree.
The global `renderToString` turns a tree into HTML (`className` and `htmlFor` map to their HTML names, `style`
objects become declarations, `dangerouslySetInnerHTML` inserts raw markup) and returns a promise when handed one.
Capitalised tags that the snippet does not declare are host functions, so a host can provide components too.

=== "Python"

    ```python
    from pydantic_monty import Monty

    with Monty(language='typescript') as pool:
        with pool.checkout(script_name='page.tsx') as session:
            tree, html = session.feed_run(
                """
    type Props = { title: string; children?: unknown };
    const Card = ({ title, children }: Props) => <section className="card"><h2>{title}</h2>{children}</section>;
    async function Users() {
      const rows: string[] = await load('users');
      return <ul>{rows.map((row) => <li key={row}>{row}</li>)}</ul>;
    }
    const tree = await <Card title="Users"><Users /></Card>;
    [tree, renderToString(tree)];
    """,
                external_lookup={'load': lambda table: ['ada', 'lin']},
            )
            print(tree['children'][1])
            """
            {
                'type': 'ul',
                'props': {},
                'children': [
                    {'type': 'li', 'props': {'key': 'ada'}, 'children': ['ada']},
                    {'type': 'li', 'props': {'key': 'lin'}, 'children': ['lin']},
                ],
            }
            """
            print(html)
            """
            <section class="card"><h2>Users</h2><ul><li>ada</li><li>lin</li></ul></section>
            """
    ```

=== "TypeScript"

    ```ts
    import { Monty } from '@pydantic/monty'

    await using pool = await Monty.create({ language: 'typescript' })
    await using session = await pool.checkout({ scriptName: 'page.tsx' })
    const [tree, html] = (await session.feedRun(
      `
    type Props = { title: string; children?: unknown };
    const Card = ({ title, children }: Props) => <section className="card"><h2>{title}</h2>{children}</section>;
    async function Users() {
      const rows: string[] = await load('users');
      return <ul>{rows.map((row) => <li key={row}>{row}</li>)}</ul>;
    }
    const tree = await <Card title="Users"><Users /></Card>;
    [tree, renderToString(tree)];
    `,
      { externalLookup: { load: async (_table: string) => ['ada', 'lin'] } },
    )) as [unknown, string]
    console.log(tree, html)
    ```

## Limits, clocks and snapshots

[Resource limits](resource-limits.md) apply unchanged: memory, feed and turn durations, recursion depth and
suspensions.
A `Date` reads the session's `OsPolicy` clock and zone, and `Math.random` its entropy policy, so a fixed clock or
seed makes a JavaScript session reproducible in the same way as a Python one.
[Snapshots](snapshots.md) capture idle and suspended JavaScript sessions too.
Type checking is not available for JavaScript or TypeScript yet; that and every other difference from Node are on
the [JavaScript limitations page](limitations/javascript.md#host-functions).
