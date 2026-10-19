// === JSON.stringify ===
console.log(JSON.stringify({ a: 1, b: 'two', c: [true, null, undefined, () => 1], d: { e: undefined }, f: NaN, g: -0 }));
console.log(JSON.stringify([undefined, function () {}, Symbol()]), JSON.stringify(undefined), JSON.stringify(null), JSON.stringify('q"\n\t\u0001'));
console.log(JSON.stringify({ nested: { deep: [1, { x: 'y' }] } }, null, 2));
console.log(JSON.stringify([1, [2, []], {}], null, '--'), JSON.stringify({ a: [] }, null, 20));
console.log(JSON.stringify({ a: 1, b: 2, c: 3 }, (key, value) => (typeof value === 'number' ? value * 2 : value)));
console.log(JSON.stringify({ toJSON() { return { replaced: true }; } }), JSON.stringify(new Number(1)), JSON.stringify(new String('s')));
console.log(JSON.stringify({ date: { toJSON: () => 'custom' } }), JSON.stringify(1e21), JSON.stringify(Infinity), JSON.stringify({ k: new Boolean(false) }));
try {
  const cyc = {};
  cyc.self = cyc;
  JSON.stringify(cyc);
} catch (err) {
  console.log(err.constructor.name, err.message);
}

// === JSON.parse ===
console.log(JSON.parse('{"a":[1,2.5,-3e2,true,null,"s\\u0041\\n"],"b":{}}'), JSON.parse(' 42 '), JSON.parse('"str"'), JSON.parse('[]'));
console.log(JSON.parse('{"a":1,"b":2}', (k, v) => (k === 'a' ? undefined : v)), JSON.parse('[1,2]', (k, v) => (Array.isArray(v) ? v : v * 10)));
console.log(JSON.parse('{"1":"one","b":"bee"}'), JSON.parse('1E2'), JSON.parse('-0'), JSON.parse('{"__proto__":1}').__proto__);
for (const bad of ['', '{', '[1,]', '{"a":1,}', "{'a':1}", '01', 'nul', '"unterminated', '1 2', '{"a" 1}', '\u0001']) {
  try {
    JSON.parse(bad);
    console.log('parsed', bad);
  } catch (err) {
    console.log(err.constructor.name, err.message);
  }
}

// === Math ===
console.log(Math.PI, Math.E, Math.abs(-3), Math.floor(-1.5), Math.ceil(-1.5), Math.round(2.5), Math.round(-2.5), Math.round(-0.4), Math.trunc(-1.7));
console.log(Math.max(), Math.min(), Math.max(1, '3', 2), Math.max(1, NaN), Math.min(0, -0), Math.sign(-3), Math.sign(0), Math.sqrt(2), Math.cbrt(27));
console.log(Math.pow(2, 0.5), Math.pow(-8, 1 / 3), 2 ** 0.5, Math.hypot(3, 4), Math.log(Math.E), Math.log2(8), Math.log10(1000), Math.exp(0));
console.log(Math.sin(0), Math.cos(Math.PI), Math.atan2(1, 1), Math.asin(1), Math.max(-0, 0), Object.is(Math.max(-0, 0), 0), Math.abs(-0));
const r = Math.random();
console.log(typeof r, r >= 0 && r < 1);

// === Number ===
console.log(Number.MAX_SAFE_INTEGER, Number.MIN_SAFE_INTEGER, Number.EPSILON > 0, Number.MAX_VALUE, Number.MIN_VALUE, Number.isInteger(5.0));
console.log(Number.isInteger('5'), Number.isNaN('NaN'), Number.isNaN(NaN), isNaN('NaN'), Number.isFinite('1'), isFinite('1'), Number.isSafeInteger(2 ** 53));
console.log((255).toString(16), (255).toString(2), (-255).toString(36), (0.5).toString(2), (1e21).toString(), (123.456).toString(10), (0).toString(2));
console.log((1.005).toFixed(2), (1234.5678).toFixed(0), (0.1).toFixed(20), (-1.5).toFixed(0), (1e21).toFixed(2), (0).toFixed(1), (-0.0001).toFixed(2));
console.log((123.456).toPrecision(2), (0.00001234).toPrecision(2), (123456).toPrecision(2), (1.5).toPrecision(), (0).toPrecision(3));
console.log(Number('12.5'), Number(true), Number(null), Number(undefined), Number([]), Number([7]), Number({}), Number('1e1000'), Number('-'));
console.log(new Number(5) + 1, typeof new Number(5), new Number(5) == 5, (5).valueOf(), Number.parseFloat === parseFloat, (25).toString(), 1e21 + 1);
for (const attempt of [() => (1).toString(1), () => (1).toFixed(-1), () => (1).toPrecision(0), () => Number.prototype.toString.call('x')]) {
  try {
    attempt();
  } catch (err) {
    console.log(err.constructor.name, err.message);
  }
}

// === Number formatting edge cases ===
console.log(0.000001, 0.0000001, 123456789012345678901234567890, 2 ** 53, 2 ** 53 + 1, 1 / 3, 100 / 3, 5e-324, 1.7976931348623157e308);
console.log(-1e-7, 1e21 - 1, 999999999999999900000, 0.1 * 3, 4.35 * 100, 1.1 + 2.2, 3 * 1.1, 1e16 + 1, 2 ** 31, 2 ** 64);
