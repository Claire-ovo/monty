"""The JavaScript sandbox through the same pool API: `Monty(language='javascript')`."""

from __future__ import annotations

import asyncio
from collections.abc import Iterator
from typing import Any

import pytest
from inline_snapshot import snapshot

from pydantic_monty import AsyncMonty, CollectString, Monty, MontyRuntimeError, MontySyntaxError


@pytest.fixture(scope='module')
def js_pool() -> Iterator[Monty]:
    with Monty(language='javascript') as pool:
        yield pool


def js_run(pool: Monty, code: str, **kwargs: Any) -> Any:
    with pool.checkout() as session:
        return session.feed_run(code, **kwargs)


def test_values_cross_the_boundary(js_pool: Monty):
    assert js_run(js_pool, '1 + 2') == snapshot(3)
    assert js_run(js_pool, "[1, 'two', { three: 3 }, null, true, 2.5]") == snapshot(
        [1, 'two', {'three': 3}, None, True, 2.5]
    )
    assert js_run(js_pool, "new Map([['a', 1]])") == snapshot({'a': 1})
    assert js_run(js_pool, 'new Set([1, 2])') == snapshot({1, 2})
    assert js_run(js_pool, 'undefined') is None


def test_inputs_and_prints(js_pool: Monty):
    output = CollectString()
    result = js_run(
        js_pool,
        "console.log('got', items, flag); items.map((x) => x * factor)",
        inputs={'items': [1, 2, 3], 'factor': 10, 'flag': True},
        print_callback=output,
    )
    assert result == snapshot([10, 20, 30])
    assert output.output == snapshot('got [ 1, 2, 3 ] true\n')


def test_session_state_persists(js_pool: Monty):
    with js_pool.checkout() as session:
        session.feed_run('let total = 0; function add(n) { total += n; return total; }')
        assert session.feed_run('add(2); add(40)') == snapshot(42)
        assert session.feed_run('typeof add') == snapshot('function')


def test_host_functions(js_pool: Monty):
    calls: list[tuple[Any, ...]] = []

    def record(*args: Any) -> int:
        calls.append(args)
        return len(calls)

    result = js_run(
        js_pool,
        "record('a', 1); record({ b: [2] }); answer * 2",
        external_lookup={'record': record, 'answer': 21},
    )
    assert result == snapshot(42)
    assert calls == snapshot([('a', 1), ({'b': [2]},)])


@pytest.mark.parametrize(
    ('code', 'exception_type', 'message'),
    [
        ('null.x', TypeError, "Cannot read properties of null (reading 'x')"),
        ('missing + 1', NameError, 'missing is not defined'),
        ("throw new Error('custom')", Exception, 'custom'),
        ('[].reduce((a, b) => a)', TypeError, 'Reduce of empty array with no initial value'),
    ],
)
def test_errors_map_to_python_exceptions(js_pool: Monty, code: str, exception_type: type[Exception], message: str):
    with pytest.raises(MontyRuntimeError) as exc_info:
        js_run(js_pool, code)
    inner = exc_info.value.exception()
    assert type(inner) is exception_type
    assert str(inner) == message
    # the session survives its feed's error
    assert js_run(js_pool, '2 + 2') == 4


def test_default_script_name_is_main_js(js_pool: Monty):
    with js_pool.checkout() as session:
        assert session.feed_run("new Error('x').stack.split('\\n')[1].includes('main.js')") is True
    with js_pool.checkout(script_name='app.js') as session:
        assert session.feed_run("new Error('x').stack.split('\\n')[1].includes('app.js')") is True


def test_unknown_language_is_rejected():
    with pytest.raises(ValueError) as exc_info:
        Monty(language='cobol')  # type: ignore[arg-type]
    assert exc_info.value.args[0] == snapshot("language must be 'python', 'javascript' or 'typescript', not 'cobol'")


def test_async_pool_runs_javascript():
    async def main() -> Any:
        async with AsyncMonty(language='javascript') as pool:
            async with pool.checkout() as session:
                return await session.feed_run('const v = await Promise.resolve(20); v + 1')

    assert asyncio.run(main()) == snapshot(21)


@pytest.fixture(scope='module')
def ts_pool() -> Iterator[Monty]:
    with Monty(language='typescript') as pool:
        yield pool


def test_typescript_pool_strips_types(ts_pool: Monty):
    code = """
interface User { name: string; age?: number }
const users: User[] = [{ name: 'ada', age: 36 }, { name: 'lin' }];
function describe<T extends User>(user: T): string {
  return `${user.name}:${user.age ?? 'unknown'}`;
}
users.map(describe) satisfies string[];
"""
    assert js_run(ts_pool, code) == snapshot(['ada:36', 'lin:unknown'])
    with ts_pool.checkout() as session:
        assert session.feed_run("new Error('x').stack.split('\\n')[1].includes('main.ts')") is True


def test_typescript_pool_rejects_enums(ts_pool: Monty):
    with pytest.raises(MontySyntaxError) as exc_info:
        js_run(ts_pool, 'enum Color { Red }')
    assert str(exc_info.value) == snapshot('TypeScript enum is not supported in strip-only mode')


def test_tsx_session_returns_a_tree(ts_pool: Monty):
    code = """
type Props = { title: string; children?: unknown };
const Card = ({ title, children }: Props) => <section className="card"><h2>{title}</h2>{children}</section>;
const tree = <Card title="Users"><ul>{names.map((name: string) => <li key={name}>{name}</li>)}</ul></Card>;
[tree, renderToString(tree)];
"""
    with ts_pool.checkout(script_name='page.tsx') as session:
        tree, html = session.feed_run(code, inputs={'names': ['ada', 'lin']})
    assert tree == snapshot(
        {
            'type': 'section',
            'props': {'className': 'card'},
            'children': [
                {'type': 'h2', 'props': {}, 'children': ['Users']},
                {
                    'type': 'ul',
                    'props': {},
                    'children': [
                        {'type': 'li', 'props': {'key': 'ada'}, 'children': ['ada']},
                        {'type': 'li', 'props': {'key': 'lin'}, 'children': ['lin']},
                    ],
                },
            ],
        }
    )
    assert html == snapshot('<section class="card"><h2>Users</h2><ul><li>ada</li><li>lin</li></ul></section>')


def test_async_component_awaits_a_host_function(ts_pool: Monty):
    code = """
async function Users() {
  const rows: string[] = await load('users');
  return <ul>{rows.map((row) => <li>{row}</li>)}</ul>;
}
await renderToString(<main><Users /></main>);
"""

    def load(table: str) -> list[str]:
        return ['ada', 'lin'] if table == 'users' else []

    with ts_pool.checkout(script_name='page.tsx') as session:
        html = session.feed_run(code, external_lookup={'load': load})
    assert html == snapshot('<main><ul><li>ada</li><li>lin</li></ul></main>')
