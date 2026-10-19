// === Closures capture bindings, not values ===
function counter() {
  let n = 0;
  return { inc: () => ++n, get: () => n };
}
const c = counter();
c.inc();
c.inc();
console.log(c.get());

// === Per-iteration let bindings ===
const fns = [];
for (let i = 0; i < 3; i++) {
  fns.push(() => i);
}
console.log(fns.map((f) => f()));

const vars = [];
for (var j = 0; j < 3; j++) {
  vars.push(() => j);
}
console.log(vars.map((f) => f()));

// === for-of / for-in bindings are fresh each iteration ===
const ofs = [];
for (const x of [10, 20]) {
  ofs.push(() => x);
}
console.log(ofs.map((f) => f()));

const ins = [];
for (const k in { a: 1, b: 2 }) {
  ins.push(() => k);
}
console.log(ins.map((f) => f()));

// === Block scope and shadowing ===
let v = 'outer';
{
  let v = 'inner';
  console.log(v);
}
console.log(v);

// === Nested closures share the same cell ===
function outer() {
  let shared = 0;
  const a = () => (shared += 1);
  const b = () => (shared += 10);
  return () => {
    a();
    b();
    return shared;
  };
}
console.log(outer()());

// === Captured parameters and arguments ===
function adder(base) {
  return (x) => base + x;
}
console.log(adder(5)(3));

function withArgs() {
  return () => arguments.length;
}
console.log(withArgs(1, 2, 3)());

// === this in arrows is lexical ===
const obj = {
  name: 'o',
  method() {
    return [1, 2].map(() => this.name);
  },
};
console.log(obj.method());

// === Named function expressions see their own name ===
const fact = function f(n) {
  return n <= 1 ? 1 : n * f(n - 1);
};
console.log(fact(5));

// === Hoisting ===
console.log(hoisted());
function hoisted() {
  return 'hoisted';
}
console.log(typeof later, later);
var later = 1;

// === TDZ ===
try {
  tdz;
  let tdz = 1;
} catch (e) {
  console.log(e.constructor.name, e.message);
}
