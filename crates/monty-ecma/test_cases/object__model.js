// === Literals ===
const key = 'dyn';
const o = { a: 1, 'b c': 2, [key + 'amic']: 3, 1: 'one', 0.5: 'half', [Symbol.iterator]: null, nested: { x: [1] } };
console.log(o, Object.keys(o), Object.values(o).length, Object.entries(o)[0]);
const short = 42;
const method = { short, fn() { return this.short; }, arrow: () => typeof this, get g() { return 'g'; }, set g(v) { this.set = v; } };
method.g = 'written';
console.log(method.short, method.fn(), method.arrow(), method.g, method.set);

// === Property access and mutation ===
o.added = true;
o['computed key'] = 1;
delete o.a;
console.log('a' in o, o.a, o.added, o['computed key'], o.missing, o?.missing?.deeper);
console.log(Object.keys({ 2: 'b', 1: 'a', z: 1, y: 2, [-1]: 'neg', '01': 'lead', 4294967295: 'max' }));

// === Spread, assign, freeze ===
const base = { x: 1, y: 2 };
const copy = { ...base, y: 3, z: 4 };
console.log(copy, base, Object.assign({}, base, { x: 9 }, null, 'ab'), Object.assign(base, { w: 0 }) === base);
const frozen = Object.freeze({ k: 1 });
console.log(Object.isFrozen(frozen), Object.isFrozen({}), Object.isFrozen(1));
try {
  frozen.k = 2;
} catch (err) {
  console.log(err.constructor.name, err.message);
}
try {
  frozen.added = 2;
} catch (err) {
  console.log(err.constructor.name, err.message);
}
try {
  delete frozen.k;
} catch (err) {
  console.log(err.constructor.name, err.message);
}

// === Prototypes ===
const proto = { greet() { return 'hi from ' + this.name; }, shared: 'proto' };
const child = Object.create(proto);
child.name = 'child';
console.log(child.greet(), child.shared, Object.getPrototypeOf(child) === proto, proto.isPrototypeOf(child), child.hasOwnProperty('shared'));
console.log(Object.keys(child), 'shared' in child, Object.hasOwn(child, 'shared'), child.propertyIsEnumerable('name'));
const nullProto = Object.create(null);
nullProto.x = 1;
console.log(nullProto, Object.getPrototypeOf(nullProto), 'toString' in nullProto);
const literalProto = { __proto__: proto, own: 1 };
console.log(literalProto.greet === proto.greet, Object.keys(literalProto));
Object.setPrototypeOf(child, null);
console.log(child.greet);

// === defineProperty and accessors ===
const defined = {};
Object.defineProperty(defined, 'hidden', { value: 1, enumerable: false });
Object.defineProperty(defined, 'visible', { value: 2, enumerable: true, writable: true });
Object.defineProperty(defined, 'computed', { get() { return this.visible * 10; }, enumerable: true });
console.log(defined, Object.keys(defined), defined.hidden, defined.computed, Object.getOwnPropertyNames(defined));
try {
  defined.hidden = 5;
} catch (err) {
  console.log(err.constructor.name, err.message);
}
console.log(defined.hidden);

// === Object.prototype methods ===
console.log(Object.prototype.toString.call([]), Object.prototype.toString.call(null), Object.prototype.toString.call(1), String({}), {} + '');
console.log(Object.fromEntries([['a', 1], ['b', 2]]), Object.entries({ x: 1 }), Object.is(1, 1), typeof Object(1), Object('s') instanceof String);

// === Identity and shared references ===
const inner = { v: 1 };
const holder = { first: inner, second: inner };
holder.first.v = 2;
console.log(holder, holder.first === holder.second, { a: 1 } === { a: 1 });

// === Symbols ===
const sym = Symbol('tag');
const withSym = { [sym]: 'symbolic', plain: 1 };
console.log(withSym[sym], Object.keys(withSym), sym.toString(), sym.description, Symbol('x') === Symbol('x'), typeof Symbol.iterator);

// === Custom iterables and toString/valueOf hooks ===
const iterable = {
  from: 1,
  to: 3,
  [Symbol.iterator]() {
    let current = this.from;
    const last = this.to;
    return { next: () => (current <= last ? { value: current++, done: false } : { value: undefined, done: true }) };
  },
};
console.log([...iterable], Array.from(iterable));
for (const v of iterable) console.log(v);
const money = { valueOf: () => 5, toString: () => 'five' };
console.log(money + 1, `${money}`, money * 2, String(money), money == 5, money < 6);
