// === Construction ===
console.log([], [1, 2, 3], new Array(3), new Array(1, 2), Array.of(7), Array.from('abc'), Array.from([1, 2], (x) => x * 2));
console.log(Array.isArray([]), Array.isArray({ length: 0 }), Array.from({ length: 3 }, (_, i) => i), [1, 2, 3].length);

// === Mutation ===
const a = [1, 2, 3];
console.log(a.push(4, 5), a, a.pop(), a.shift(), a.unshift(0), a, a.splice(1, 2), a, a.splice(1, 0, 'x', 'y'), a);
console.log(a.reverse(), a.fill(0, 1, 3), [3, 1, 2].sort(), [10, 9, 1].sort(), [10, 9, 1].sort((x, y) => x - y));
console.log(['b', undefined, 'a', , 'c'].sort(), [3, 1, 2].sort((x, y) => y - x));
a.length = 2;
console.log(a, a.length);
a[5] = 'far';
console.log(a, a.length);

// === Access ===
const b = [10, 20, 30, 40];
console.log(b[0], b[-1], b[10], b.at(-1), b.at(1), b.at(10), b.indexOf(30), b.indexOf(99), b.lastIndexOf(10));
console.log(b.includes(20), b.includes('20'), [NaN].includes(NaN), [NaN].indexOf(NaN), b.slice(1, 3), b.slice(-2), b.slice());
console.log(b.join(), b.join('-'), b.join(''), [null, undefined, 1].join(), [[1, 2], [3]].join(';'), b.toString(), String(b));

// === Iteration ===
console.log(b.map((x) => x / 10), b.filter((x) => x > 15), b.find((x) => x > 15), b.findIndex((x) => x > 15));
console.log(b.findLast((x) => x < 35), b.findLastIndex((x) => x < 35), b.some((x) => x > 35), b.every((x) => x > 5));
console.log(b.reduce((s, x) => s + x), b.reduce((s, x) => s + x, 100), b.reduceRight((s, x) => s + '|' + x));
const seen = [];
b.forEach((x, i, arr) => seen.push([x, i, arr.length]));
console.log(seen);
console.log([1, [2, [3, [4]]]].flat(), [1, [2, [3, [4]]]].flat(Infinity), [1, 2].flatMap((x) => [x, x * 10]));
console.log([1, 2].concat(3, [4, 5], [[6]]), [...b.keys()], [...b.entries()], [...b.values()]);
console.log(Array.from(b.entries()).map((e) => e[0] * e[1]));

// === Holes ===
const holes = [1, , 3];
console.log(holes, holes.length, 1 in holes, holes.map((x) => x * 2), holes.indexOf(undefined), holes.includes(undefined));
console.log(holes.filter(() => true), Object.keys(holes), holes.join('-'), JSON.stringify(holes));

// === Nested and shared references ===
const shared = [1];
const nested = [shared, shared];
nested[0].push(2);
console.log(nested, nested[0] === nested[1]);

// === Destructive callbacks ===
const grow = [1, 2, 3];
const visited = [];
grow.forEach((x) => {
  visited.push(x);
  if (x === 1) grow.push(99);
});
console.log(visited, grow);

// === Array-likes and spread of strings ===
console.log([...'héy'], Array.from({ 0: 'a', 1: 'b', length: 2 }), Math.max(...[1, 5, 3]), [...[1, 2], ...[]]);

// === Errors ===
try {
  [].reduce((x) => x);
} catch (err) {
  console.log(err.constructor.name, err.message);
}
try {
  [1].map(5);
} catch (err) {
  console.log(err.constructor.name, err.message);
}
try {
  const arr = [];
  arr.length = -1;
} catch (err) {
  console.log(err.constructor.name, err.message);
}
