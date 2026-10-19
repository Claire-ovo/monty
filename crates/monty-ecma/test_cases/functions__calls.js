// === Defaults, rest and spread ===
function greet(name = 'world', punctuation = '!') {
  return `hello ${name}${punctuation}`;
}
console.log(greet(), greet('there'), greet(undefined, '?'), greet(null));

function sum(...nums) {
  return nums.reduce((a, b) => a + b, 0);
}
console.log(sum(), sum(1), sum(1, 2, 3), sum(...[4, 5], 6));

function lengths(a, b = 2, ...rest) {}
console.log(greet.length, sum.length, lengths.length, ((a, b) => {}).length);
console.log(greet.name, sum.name, (() => {}).name, (function () {}).name, Math.max.name);

// === arguments ===
function countArgs() {
  return arguments.length + ':' + Array.from(arguments).join(',');
}
console.log(countArgs(), countArgs(1, 'two', [3]));

// === call / apply / bind ===
function whoAmI(prefix, suffix) {
  return prefix + this.name + (suffix || '');
}
const alice = { name: 'alice' };
console.log(whoAmI.call(alice, '<', '>'), whoAmI.apply(alice, ['[', ']']), whoAmI.bind(alice, '(')(')'));
const bound = whoAmI.bind({ name: 'bob' });
console.log(bound('*'), bound.name, bound.length);

// === Recursion and mutual recursion ===
function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}
console.log(fib(20));
function isEven(n) {
  return n === 0 ? true : isOdd(n - 1);
}
function isOdd(n) {
  return n === 0 ? false : isEven(n - 1);
}
console.log(isEven(10), isOdd(7));

// === Higher-order functions and immediate invocation ===
const compose = (...fns) => (x) => fns.reduceRight((acc, f) => f(acc), x);
console.log(compose((x) => x + 1, (x) => x * 2)(5));
console.log((function () { return 'iife'; })(), (() => 'arrow iife')());

// === Method calls keep this; detached methods lose it ===
const counter = {
  n: 0,
  bump() {
    this.n++;
    return this;
  },
};
console.log(counter.bump().bump().n);
const detached = counter.bump;
try {
  detached();
} catch (e) {
  console.log(e.constructor.name, e.message);
}

// === Calling non-functions ===
try {
  const notFn = 5;
  notFn();
} catch (e) {
  console.log(e.constructor.name, e.message);
}
try {
  const o = {};
  o.missing();
} catch (e) {
  console.log(e.constructor.name, e.message);
}
try {
  undefined.prop;
} catch (e) {
  console.log(e.constructor.name, e.message);
}

// === Constructors ===
function Point(x, y) {
  this.x = x;
  this.y = y;
}
Point.prototype.len = function () {
  return Math.hypot(this.x, this.y);
};
const p = new Point(3, 4);
console.log(p, p.len(), p instanceof Point, p.constructor === Point, Object.keys(p));
function Explicit() {
  return { replaced: true };
}
console.log(new Explicit());
try {
  new ((x) => x)();
} catch (e) {
  console.log(e.constructor.name, e.message);
}

// === Stack depth is bounded ===
function forever() {
  return forever();
}
try {
  forever();
} catch (e) {
  console.log(e.constructor.name, e.message);
}

// === Bound functions: instanceof and repeated calls ===
function Pt() {}
const BoundPoint = Pt.bind(null);
const ReboundPoint = BoundPoint.bind(null);
const point = new Pt();
let hits = 0;
for (let i = 0; i < 50; i++) {
  if (point instanceof BoundPoint && point instanceof ReboundPoint) hits++;
}
console.log(hits, Pt.name, new BoundPoint() instanceof Pt, [] instanceof BoundPoint);
function Crate(v) {
  this.v = v;
}
function addTo(a, b) {
  return this.base + a + b;
}
let total = 0;
for (let i = 0; i < 2000; i++) {
  const addI = addTo.bind({ base: 1 }, i);
  total += addI(1) + new (Crate.bind(null, i))().v;
}
console.log(total);
