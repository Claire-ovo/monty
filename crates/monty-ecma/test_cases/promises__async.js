// === Ordering: synchronous code, microtasks, then timers ===
console.log('start');
setTimeout(() => console.log('timeout 0'), 0);
setTimeout(() => console.log('timeout 10'), 10);
setTimeout((a, b) => console.log('timeout args', a, b), 5, 'x', 'y');
Promise.resolve().then(() => console.log('micro 1'));
queueMicrotask(() => console.log('micro 2'));
Promise.resolve()
  .then(() => console.log('micro 3'))
  .then(() => console.log('micro 4'));
(async () => {
  console.log('async body sync part');
  await null;
  console.log('after await');
})();
console.log('end');

// === Promise basics ===
const resolved = Promise.resolve(42);
const pending = new Promise(() => {});
const rejected = Promise.reject('nope');
rejected.catch(() => {});
console.log(resolved, pending, rejected, typeof resolved.then, resolved instanceof Promise, Object.prototype.toString.call(resolved));
console.log(Promise.resolve(resolved) === resolved, Promise.name, Promise.length, Promise.prototype.then.length, String(resolved));
new Promise((resolve, reject) => {
  resolve('first');
  resolve('second');
  reject(new Error('ignored'));
}).then((v) => console.log('settled once:', v));
new Promise((resolve) => resolve(Promise.resolve('nested'))).then((v) => console.log('unwrapped', v));
Promise.resolve({ then(res) { res('thenable'); } }).then((v) => console.log(v));
Promise.resolve(1)
  .then((v) => v + 1)
  .then((v) => { throw new Error('boom ' + v); })
  .then(() => console.log('skipped'))
  .catch((err) => { console.log('caught', err.message); return 'recovered'; })
  .finally(() => console.log('finally runs'))
  .then((v) => console.log('after finally', v));
Promise.reject('plain')
  .finally(() => 'ignored')
  .catch((v) => console.log('finally passes rejection', v));
Promise.resolve('kept')
  .finally(() => { throw new Error('finally throws'); })
  .catch((err) => console.log(err.message));
Promise.resolve(1).then(2, 3).then((v) => console.log('non-callable handlers pass through', v));
new Promise(() => { throw new Error('executor throws'); }).catch((err) => console.log(err.message));
const self = new Promise((resolve) => setTimeout(() => resolve(self), 1));
self.catch((err) => console.log(err.constructor.name, err.message));

// === Combinators ===
Promise.all([1, Promise.resolve(2), new Promise((r) => setTimeout(() => r(3), 2))]).then((v) => console.log('all', v));
Promise.all([]).then((v) => console.log('all empty', v));
Promise.all([Promise.resolve(1), Promise.reject(new Error('all fails'))]).catch((err) => console.log(err.message));
Promise.allSettled([Promise.resolve(1), Promise.reject('r'), 3]).then((v) => console.log('allSettled', v));
Promise.race([new Promise((r) => setTimeout(() => r('slow'), 20)), new Promise((r) => setTimeout(() => r('fast'), 3))]).then((v) => console.log('race', v));
Promise.race([]).then(() => console.log('never'));
Promise.any([Promise.reject(new Error('a')), Promise.resolve('any wins'), Promise.reject(new Error('b'))]).then((v) => console.log(v));
Promise.any([Promise.reject(new Error('a')), Promise.reject(new Error('b'))]).catch((err) => console.log(err.constructor.name, err.message, err.errors.map((e) => e.message)));
Promise.any([]).catch((err) => console.log(err.constructor.name, err.errors));
const { promise, resolve } = Promise.withResolvers();
promise.then((v) => console.log('withResolvers', v));
resolve('done');
Promise.try(() => 'sync value').then((v) => console.log('try', v));
Promise.try(() => { throw new Error('try throws'); }).catch((err) => console.log(err.message));
Promise.try((a, b) => a + b, 2, 3).then((v) => console.log('try args', v));

// === async functions ===
async function add(a, b) {
  const x = await a;
  const y = await b;
  return x + y;
}
const sum = add(Promise.resolve(1), 2);
console.log(sum, add, add.name, add.length, Object.getPrototypeOf(add) === Function.prototype, Object.prototype.toString.call(add));
sum.then((v) => console.log('sum', v));
async function fails() {
  await null;
  throw new TypeError('async throw');
}
fails().catch((err) => console.log(err.constructor.name, err.message));
async function catches() {
  try {
    await Promise.reject(new Error('rejected await'));
  } catch (err) {
    console.log('caught in async', err.message);
    return 'handled';
  } finally {
    console.log('async finally');
  }
}
catches().then((v) => console.log(v));
const arrow = async (v) => (await v) * 2;
arrow(21).then((v) => console.log('arrow', v));
class Service {
  constructor() {
    this.base = 10;
  }
  async method(v) {
    await null;
    return this.base + v;
  }
  static async create() {
    const s = new Service();
    await s.method(0);
    return s;
  }
}
new Service().method(5).then((v) => console.log('method', v));
Service.create().then((s) => console.log('static', s.base));
const obj = {
  value: 'obj',
  async shorthand() {
    return this.value;
  },
};
obj.shorthand().then((v) => console.log('shorthand', v));
async function loops() {
  let total = 0;
  for (const v of [1, 2, 3]) total += await v;
  for (let i = 0; i < 2; i++) await null;
  return total;
}
loops().then((v) => console.log('loops', v));
async function nested() {
  const inner = async () => {
    await null;
    return 'inner';
  };
  return (await inner()) + ' outer';
}
nested().then(console.log);
const order = [];
async function a1() {
  order.push('a1 start');
  await a2();
  order.push('a1 end');
}
async function a2() {
  order.push('a2');
}
a1().then(() => console.log(order));
order.push('sync after a1');
async function returnsPromise() {
  return Promise.resolve('flattened');
}
returnsPromise().then((v) => console.log(v));
console.log(async () => {}, async function named() {}, (async function () {}).constructor === Function);
try {
  new add();
} catch (err) {
  console.log(err.constructor.name + ': ' + err.message);
}

// === Timers ===
const interval = setInterval(() => {
  ticks++;
  console.log('tick', ticks);
  if (ticks === 3) clearInterval(interval);
}, 15);
let ticks = 0;
const cancelled = setTimeout(() => console.log('never printed'), 1);
clearTimeout(cancelled);
clearTimeout(undefined);
setImmediate((v) => console.log('immediate', v), 'arg');
for (const attempt of [() => setTimeout('string'), () => queueMicrotask(5), () => new Promise(), () => Promise(), () => Promise.prototype.then.call({})]) {
  try {
    attempt();
  } catch (err) {
    console.log(err.constructor.name + ': ' + err.message);
  }
}

// === Top-level await ===
const tla = await new Promise((r) => setTimeout(() => r('top-level await'), 100));
console.log(tla);
console.log(await Promise.all([add(1, 2), arrow(4)]));
try {
  await fails();
} catch (err) {
  console.log('tla caught', err.message);
}
console.log('done');
