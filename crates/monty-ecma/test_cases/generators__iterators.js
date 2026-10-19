// === Basics ===
function* count(limit) {
  for (let i = 0; i < limit; i++) {
    yield i;
  }
  return 'done';
}
const gen = count(3);
console.log(gen, typeof gen, gen.next(), gen.next(), gen.next(), gen.next(), gen.next());
console.log([...count(4)], Array.from(count(2)), Math.max(...count(5)));
for (const n of count(2)) console.log('for-of', n);
const [first, second, ...others] = count(5);
console.log(first, second, others);
console.log(count, count.name, count.length, typeof count, count.prototype, Object.getPrototypeOf(count) === Function.prototype);
console.log(Object.getPrototypeOf(gen) === count.prototype, gen instanceof count, Object.prototype.toString.call(gen), String(gen));
console.log(Object.getPrototypeOf(count.prototype) === Object.getPrototypeOf(Object.getPrototypeOf((function* () {})())), count.prototype.constructor === undefined);
console.log(gen[Symbol.iterator]() === gen, typeof gen.next, typeof gen.return, typeof gen.throw, Object.keys(gen), 'next' in gen);

// === Two-way communication ===
function* echo() {
  const a = yield 'first';
  const b = yield a * 2;
  return a + b;
}
const e = echo();
console.log(e.next('ignored'), e.next(5), e.next(7), e.next(9));

// === Parameters evaluate at call time, body runs lazily ===
let calls = 0;
function* lazy(a = (calls += 1, 'default')) {
  calls += 10;
  yield a;
}
const l = lazy();
console.log(calls, l.next(), calls, lazy('given').next());

// === return() and throw() ===
function* guarded() {
  try {
    yield 1;
    yield 2;
  } catch (err) {
    console.log('caught', err);
    yield 'recovered';
  } finally {
    console.log('finally');
  }
  yield 'after';
}
let g = guarded();
console.log(g.next(), g.throw('boom'), g.next(), g.next());
g = guarded();
console.log(g.next(), g.return('early'), g.next(), g.return('again'), g.throw !== undefined);
g = guarded();
console.log(g.return('before start'), g.next());
g = guarded();
try {
  g.throw(new Error('before start'));
} catch (err) {
  console.log(err.message, g.next());
}
function* overriding() {
  try {
    yield 1;
  } finally {
    yield 'from finally';
    return 'finally wins';
  }
}
g = overriding();
console.log(g.next(), g.return('x'), g.next(), g.next());
try {
  const t = (function* () {
    yield 1;
  })();
  t.next();
  t.throw(new TypeError('thrown out'));
} catch (err) {
  console.log(err.constructor.name, err.message);
}

// === Generator state ===
function* reentrant() {
  try {
    self.next();
  } catch (err) {
    yield err.message;
  }
}
const self = reentrant();
console.log(self.next(), self.next());
function* once() {
  yield 1;
}
const o = once();
console.log(o.next(), o.next(), o.next(), o.return(5), o.throw === Object.getPrototypeOf(o).throw);

// === yield* delegation ===
function* inner() {
  const got = yield 'i1';
  console.log('inner got', got);
  yield 'i2';
  return 'inner result';
}
function* outer() {
  const result = yield* inner();
  console.log('outer saw', result);
  yield* [10, 20];
  yield* 'ab';
  yield* new Set([true]);
  return 'outer done';
}
const d = outer();
console.log(d.next(), d.next('sent'), d.next(), d.next(), d.next(), d.next(), d.next(), d.next(), d.next());
function* delegatesErrors() {
  try {
    yield* inner();
  } catch (err) {
    yield 'outer caught ' + err;
  }
}
g = delegatesErrors();
console.log(g.next(), g.throw('err'), g.next());
const custom = {
  [Symbol.iterator]() {
    return {
      next: (v) => ({ value: 'custom ' + v, done: false }),
      return: (v) => {
        console.log('custom return', v);
        return { value: 'ret ' + v, done: true };
      },
    };
  },
};
g = (function* () {
  const r = yield* custom;
  console.log('unreachable', r);
})();
console.log(g.next('a'), g.next('b'), g.return('stop'), g.next());
const noThrow = {
  [Symbol.iterator]() {
    return {
      next: () => ({ value: 1, done: false }),
      return: () => {
        console.log('closed');
        return {};
      },
    };
  },
};
g = (function* () {
  yield* noThrow;
})();
g.next();
try {
  g.throw(new Error('x'));
} catch (err) {
  console.log(err.constructor.name + ': ' + err.message);
}

// === Generators as methods and in objects ===
class Tree {
  constructor(...children) {
    this.children = children;
  }
  *[Symbol.iterator]() {
    yield this.children.length;
    for (const child of this.children) yield* child;
  }
  static *range(n) {
    for (let i = 0; i < n; i++) yield i;
  }
}
console.log([...new Tree(new Tree(), new Tree(new Tree()))], [...Tree.range(3)]);
const obj = {
  *pairs() {
    for (const key of Object.keys(this)) yield [key, this[key]];
  },
  x: 1,
  y: 2,
};
console.log([...obj.pairs()], obj.pairs, Object.getPrototypeOf(obj.pairs) !== Function.prototype);
const fibonacci = function* () {
  let [a, b] = [0, 1];
  while (true) {
    yield a;
    [a, b] = [b, a + b];
  }
};
const fib = fibonacci();
console.log(Array.from({ length: 10 }, () => fib.next().value), fibonacci);
function* take(iterable, n) {
  let i = 0;
  for (const item of iterable) {
    if (i++ >= n) return;
    yield item;
  }
}
console.log([...take(fibonacci(), 7)], [...take([1, 2, 3], 10)]);

// === Closures over generator state, arguments and this ===
function* closures() {
  const fns = [];
  for (let i = 0; i < 3; i++) {
    yield fns.push(() => i * 10);
  }
  return fns.map((f) => f());
}
const c = closures();
c.next();
c.next();
c.next();
console.log(c.next());
function* args() {
  yield arguments.length;
  yield [...arguments];
}
console.log([...args(1, 2, 3)]);
console.log([...(function* () { yield this; }).call('ctx')]);

// === Errors ===
const attempts = [
  () => new count(),
  () => Object.getPrototypeOf(count.prototype).next.call({}),
  () => count.prototype.next.call(5),
  () => [...(function* () { yield* 5; })()],
  () => (function* () {}).prototype.next(),
];
for (const attempt of attempts) {
  try {
    console.log(attempt());
  } catch (err) {
    console.log(err.constructor.name + ': ' + err.message);
  }
}
