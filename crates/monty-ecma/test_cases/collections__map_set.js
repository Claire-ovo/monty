// === Map: SameValueZero keys, insertion order, size and deletion ===
const m = new Map([[1, 'one'], ['1', 'string one'], [NaN, 'nan'], [{}, 'obj']]);
m.set(-0, 'zero').set(undefined, 'u');
console.log(m, m.size, m.get(1), m.get('1'), m.get(NaN), m.get(0), m.has(undefined), m.get({}), m.delete(1), m.delete(99), m.size);
console.log([...m.keys()], [...m.values()], [...m.entries()].length, [...m].length, new Map(), new Map().size);
m.forEach((v, k, map) => console.log(k, v, map === m));
// === Set ===
const s = new Set([1, 2, 2, '2', NaN, NaN, -0, 0]);
console.log(s, s.size, s.has(2), s.has('2'), s.has(NaN), s.add(3), s.delete(1), s.delete(1), [...s], [...s.entries()][0]);
console.log(new Set('hello'), new Set(), Array.from(new Set([3, 1, 3])), new Set([1]).values().next());
// === Nesting, tags, JSON and long collections ===
const nested = new Map([['k', new Map([['inner', new Set([1])]])]]);
console.log(nested, { m: new Map([[1, { a: [1] }]]) }, [new Set([new Map()])]);
console.log(String(new Map()), Object.prototype.toString.call(new Set()), typeof Map, Map.name, Map.prototype.constructor === Map, new Map() instanceof Map);
console.log(JSON.stringify(new Map([[1, 2]])), JSON.stringify({ s: new Set([1]) }), Object.keys(new Map([[1, 2]])));
const big = new Map(); for (let i = 0; i < 120; i++) big.set(i, i * i);
console.log(big);
const bigs = new Set(); for (let i = 0; i < 120; i++) bigs.add('item' + i);
console.log(bigs);
// === Errors and weak collections ===
try { new Map([1]); } catch (e) { console.log(e.constructor.name, e.message); }
try { Map(); } catch (e) { console.log(e.constructor.name, e.message); }
try { Map.prototype.get.call({}, 1); } catch (e) { console.log(e.constructor.name, e.message); }
try { new Map(5); } catch (e) { console.log(e.constructor.name, e.message); }
const wm = new WeakMap(); const key = {}; wm.set(key, 1); console.log(wm.get(key), wm.has({}), wm, new WeakSet([key]).has(key), new WeakSet());
try { wm.set(1, 1); } catch (e) { console.log(e.constructor.name, e.message); }
const iter = m.entries(); console.log(iter, iter[Symbol.iterator]() === iter, Object.prototype.toString.call(iter), iter.next());
const grouped = Map.groupBy ? 'has groupBy' : 'no'; console.log(grouped, Object.groupBy ? 'has' : 'no');
console.log(new Map([[1,2]]).set(3,4).get(3), new Set([1]).add(2).size, m.clear(), m.size, s.clear(), s.size);
