// === Arithmetic ===
console.log(7 / 2, 7 % 3, -7 % 3, 2 ** 10, 2 ** -1, 1 / 0, -1 / 0, 0 / 0);
console.log(0.1 + 0.2, 1e300 * 1e10, 5 - '2', '5' * '2', '5' + 2, 5 + '2', '3' - - '1');
console.log(1 + null, 1 + undefined, 1 + true, 'a' + null, [] + [], [] + {}, [1, 2] + [3]);
console.log(+'', +' 12 ', +'0x1f', +'1e3', +'abc', +[], +[5], +[1, 2], +null, +undefined, +true);

// === Bitwise and shifts ===
console.log(5 & 3, 5 | 3, 5 ^ 3, ~5, 1 << 31, 1 << 32, -1 >> 1, -1 >>> 0, -1 >>> 28, 2 ** 32 | 0);
console.log(1.9 | 0, -1.9 | 0, NaN | 0, Infinity | 0, '12' | 0);

// === Increment and decrement ===
let n = 1;
console.log(n++, n, ++n, n--, n, --n);
const obj = { p: 5 };
console.log(obj.p++, obj.p, --obj.p, obj['p']++, obj.p);
let s = '5';
console.log(s++, s, typeof s);

// === Compound assignment ===
let x = 10;
x += 5;
x -= 3;
x *= 2;
x /= 4;
x %= 4;
x **= 3;
x <<= 1;
x |= 1;
console.log(x);
let str = 'a';
str += 1;
str += null;
console.log(str);
let u;
u ??= 'set';
u ||= 'ignored';
u &&= 'and';
console.log(u);
const o = { count: 0 };
o.count += 1;
o.count ||= 99;
o.missing ??= 'filled';
console.log(o);

// === Equality ===
console.log(1 == '1', 1 === '1', null == undefined, null === undefined, NaN == NaN, NaN === NaN);
console.log(0 == '', 0 == '0', '' == '0', false == 'false', false == '0', [] == false, [1] == 1);
console.log(null == 0, null >= 0, undefined == 0, {} == {}, '1,2' == [1, 2], 'a' == new String('a'));
console.log(0 === -0, Object.is(0, -0), Object.is(NaN, NaN));

// === Relational ===
console.log(2 < 12, '2' < '12', '2' < 12, 'a' < 'b', 'B' < 'a', null < 1, undefined < 1, NaN < 1);
console.log([2] > 1, 'abc' <= 'abd', '' < 'a', 1 <= '1', 1 >= '1');

// === typeof, void, delete, in, instanceof ===
console.log(typeof 1, typeof 'a', typeof true, typeof undefined, typeof null, typeof {}, typeof [], typeof (() => {}));
console.log(typeof Symbol(), typeof notDeclared, void 0, void 'x');
const d = { a: 1, b: 2 };
console.log(delete d.a, d, 'b' in d, 'a' in d, 0 in [1], 'length' in [], delete d.nope);
console.log([] instanceof Array, [] instanceof Object, {} instanceof Array, (() => {}) instanceof Function);

// === Truthiness ===
console.log(!!'', !!'0', !!0, !!-0, !!NaN, !!null, !!undefined, !![], !!{}, !!'false');
console.log(1 && 2, 0 && 2, 1 || 2, 0 || 2, null ?? 0, 0 ?? 1, !0 + !0);

// === Conditional and precedence ===
console.log(1 + 2 * 3 ** 2, (1 + 2) * 3, -(2 ** 2), 2 * -3, 10 - 2 - 3, 2 ** 3 ** 2, 1 < 2 < 3, 3 > 2 > 1);
console.log(true ? 'yes' : 'no', 0 ? 'yes' : null ? 'inner' : 'outer');

// === String conversion ===
console.log(String(123), String(null), String(undefined), String(true), String([1, [2, 3]]), String({}));
console.log(`${1}${'2'}${null}${undefined}${[3, 4]}${{}}`, '' + 1.0, '' + 1.5e300, '' + -0, '' + 2e-7);
console.log(Number('  42  '), Number(''), Number('1_000'), Number('0b101'), Number('0o17'), Number('-0x10'));
console.log(parseInt('42px'), parseInt('  0x1A'), parseInt('101', 2), parseInt('z', 36), parseInt(''), parseFloat('3.14abc'));
console.log(parseInt('08'), parseFloat('.5'), parseFloat('-.5e1'), parseFloat('Infinityx'), isNaN('abc'), isFinite('12'));
