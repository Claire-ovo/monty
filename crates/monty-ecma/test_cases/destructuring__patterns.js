// === Array patterns ===
const [a, b, , d = 4, ...rest] = [1, 2, 3, undefined, 5, 6];
console.log(a, b, d, rest);
const [[deep], [[deeper]]] = [['d'], [['e']]];
console.log(deep, deeper);
const [c1, c2] = 'hi';
console.log(c1, c2);
const [x = 1, y = x + 1, [z] = [9]] = [];
console.log(x, y, z);
const [onlyRest, ...none] = [1];
console.log(onlyRest, none);

// === Object patterns ===
const { m = 'dm', n: { o = 'do' } = {}, ...others } = { m: undefined, p: 1, q: 2 };
console.log(m, o, others);
const key = 'k';
const { [key]: kv, [`${key}2`]: kv2 = 'd2', 'quoted-key': qk, 42: num } = { k: 'kval', 'quoted-key': 'q', 42: 'n' };
console.log(kv, kv2, qk, num);
const { length } = 'hello';
console.log(length);
const { toFixed } = 1.5;
console.log(typeof toFixed);
const { aa, bb = aa * 2, cc: { dd } = { dd: aa + bb } } = { aa: 3 };
console.log(aa, bb, dd);

// === Parameters ===
function f({ a, b = 2 } = {}, [c, ...d] = [3, 4, 5], ...rest) {
  return [a, b, c, d, rest];
}
console.log(f(), f({ a: 1 }, [7], 8, 9), f.length);
const arrow = ([first], { second }) => first + second;
console.log(arrow(['x'], { second: 'y' }));
function g(...[r1, r2]) {
  return r2 + r1;
}
console.log(g(1, 2, 3));

// === Assignment patterns ===
let s1 = 1;
let s2 = 2;
[s1, s2] = [s2, s1];
console.log(s1, s2);
const obj = {};
[obj.a, obj['b']] = [1, 2];
({ c: obj.c = 3, d: obj.d } = { d: 4 });
console.log(obj);
let v1;
let v2;
const result = ([v1, v2] = [10, 20]);
console.log(v1, v2, result);
let w1;
let w2;
({ w1, w2 = 'dw' } = { w1: 'given' });
console.log(w1, w2);
let r;
[, , r] = 'abc';
console.log(r);

// === Loop heads ===
for (const [i, v] of ['a', 'b'].entries()) console.log(i, v);
for (const { name, age = 0 } of [{ name: 'n' }, { name: 'm', age: 1 }]) console.log(name, age);
let li;
let lv;
for ([li, lv] of [[1, 'one']]) console.log(li, lv);
for (const [ch] of 'xy') console.log(ch);

// === Catch patterns ===
try {
  throw { code: 42, detail: 'd' };
} catch ({ code, detail }) {
  console.log(code, detail);
}
try {
  throw new RangeError('ranged');
} catch ({ message, name }) {
  console.log(name, message);
}

// === Iterator protocol: return() only when left mid-way ===
let log = [];
const it = {
  [Symbol.iterator]() {
    let i = 0;
    return {
      next: () => ({ value: i, done: i++ >= 5 }),
      return() {
        log.push('return');
        return {};
      },
    };
  },
};
const [p, q] = it;
console.log(p, q, log);
log = [];
const [r1, r2, r3, r4, r5, r6] = it;
console.log(r6, log);
log = [];
const [...all] = it;
console.log(all, log);

// === Errors ===
const attempts = [
  () => {
    const { a } = null;
  },
  () => {
    const { a } = undefined;
  },
  () => {
    const {} = null;
  },
  () => {
    const [a] = null;
  },
  () => {
    const [a] = 5;
  },
  () => {
    const [a] = {};
  },
  () => {
    const { a: { b } } = { a: null };
  },
  () => {
    let x;
    ({ x } = undefined);
  },
  () => {
    let x;
    [x] = 1;
  },
  () => {
    function h({ a }) {}
    h();
  },
  () => {
    function h([a]) {}
    h(null);
  },
  () => {
    const { ...rest } = null;
  },
];
for (const attempt of attempts) {
  try {
    attempt();
    console.log('no error');
  } catch (err) {
    console.log(err.constructor.name + ': ' + err.message);
  }
}
