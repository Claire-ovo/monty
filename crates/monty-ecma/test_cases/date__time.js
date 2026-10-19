// === Construction and inspection ===
const d = new Date(Date.UTC(2026, 9, 19, 22, 16, 11, 123));
console.log(d, [d], { d }, String(d), d.toISOString(), d.toJSON(), d.toUTCString(), d.toDateString(), d.toTimeString());
console.log(d.getTime(), d.valueOf(), +d, d.getTimezoneOffset(), d.getFullYear(), d.getMonth(), d.getDate(), d.getDay(), d.getHours(), d.getMinutes(), d.getSeconds(), d.getMilliseconds());
console.log(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), d.getUTCDay(), d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds(), d.getUTCMilliseconds());
console.log(new Date(0), new Date(-1), new Date(8.64e15), new Date(8.64e15 + 1), new Date(NaN), String(new Date(NaN)), new Date(2020, 0, 31, 25, 61), new Date(99, 0), new Date(1999, 11, 31, 23, 59, 59, 999));
console.log(new Date('2020-01-02'), new Date('2020-01-02T03:04:05'), new Date('2020-01-02T03:04:05Z'), new Date('2020-01-02T03:04:05.5+02:00'), new Date('2020-02-30'), new Date('2020'), new Date('2020-06'), new Date('+010000-01-01T00:00:00Z'));
console.log(new Date('Tue Oct 20 2026 00:00:00 GMT+0000'), new Date('Mon, 19 Oct 2026 22:16:11 GMT'), new Date('nonsense'), new Date('2020-13-01'), new Date(new Date(5)), Date.parse('2020-01-02'), Date.parse('garbage'));
console.log(typeof Date(), Date.length, Date.now.length, Date.prototype.constructor === Date, Object.prototype.toString.call(d), typeof Date.now(), Date.now() > 1_700_000_000_000);

// === Coercion and comparison ===
console.log(d[Symbol.toPrimitive]('number'), d[Symbol.toPrimitive]('string') === d.toString(), `${d}` === d.toString(), d + 1 === d.toString() + '1', d - 1, d > new Date(0), new Date(2020, 0) - new Date(2019, 0), JSON.stringify({ d, bad: new Date(NaN) }), new Date(5) == 5, new Date(5) == new Date(5));
try {
  new Date(NaN).toISOString();
} catch (err) {
  console.log(err.constructor.name, err.message);
}
try {
  d[Symbol.toPrimitive]('other');
} catch (err) {
  console.log(err.constructor.name, err.message);
}
try {
  Date.prototype.getTime.call({});
} catch (err) {
  console.log(err.constructor.name, err.message);
}

// === Setters ===
const s = new Date(Date.UTC(2020, 0, 31));
console.log(s.setMonth(1), s, s.setDate(0), s, s.setFullYear(2021, 5, 15), s, s.setHours(25, 70, 90, 1500), s);
console.log(s.setUTCMilliseconds(999), s.setUTCSeconds(1, 2), s.setUTCMinutes(3), s.setUTCHours(4), s.setUTCDate(5), s.setUTCMonth(6), s.setUTCFullYear(2022), s, s.setTime(0), s);
console.log(new Date(NaN).setFullYear(2000), new Date(NaN).setMonth(1), new Date(0).setMilliseconds(), new Date(0).setMinutes(NaN));
