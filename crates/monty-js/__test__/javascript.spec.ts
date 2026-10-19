// The JavaScript sandbox through the same pool API: `Monty.create({ language: 'javascript' })`.

import { afterAll, beforeAll, test } from 'vitest'
import { t } from './assertions.js'

import { type CheckoutOptions, type FeedOptions, Monty } from '@pydantic/monty'

let pool: Monty

beforeAll(async () => {
  pool = await Monty.create({ language: 'javascript' })
})
afterAll(async () => {
  await pool.close()
})

async function run(code: string, checkout: CheckoutOptions = {}, feed: FeedOptions = {}): Promise<unknown> {
  const session = await pool.checkout(checkout)
  try {
    return await session.feedRun(code, feed)
  } finally {
    await session.close()
  }
}

test('values cross the boundary', async () => {
  t.is(await run('1 + 2'), 3)
  // plain objects cross as dicts, which the host sees as `Map`s (as Python dicts do)
  t.deepEqual(await run("[1, 'two', { three: 3 }, null, true]"), [1, 'two', new Map([['three', 3]]), null, true])
  t.deepEqual(await run("new Map([['a', 1]])"), new Map([['a', 1]]))
})

test('inputs, prints and state', async () => {
  const printed: string[] = []
  const result = await run(
    "console.log('got', items); items.map((x) => x * 2)",
    {},
    { inputs: { items: [1, 2] }, printCallback: (_stream, text) => printed.push(text) },
  )
  t.deepEqual(result, [2, 4])
  t.deepEqual(printed, ['got [ 1, 2 ]\n'])
  const session = await pool.checkout()
  try {
    await session.feedRun('let total = 40')
    t.is(await session.feedRun('total += 2; total'), 42)
  } finally {
    await session.close()
  }
})

test('host functions and errors', async () => {
  const calls: unknown[] = []
  t.is(
    await run("record('a'); record({ b: 1 }); answer + 1", {}, {
      externalLookup: {
        record: (...args: unknown[]) => {
          calls.push(args)
          return calls.length
        },
        answer: 41,
      },
    }),
    42,
  )
  t.deepEqual(calls, [['a'], [new Map([['b', 1]])]])
  await t.throwsAsync(run('null.x'), { message: "TypeError: Cannot read properties of null (reading 'x')" })
})

test('the default script name is main.js', async () => {
  t.is(pool.language, 'javascript')
  t.is(await run("new Error('x').stack.split('\\n')[1].includes('main.js')"), true)
})
