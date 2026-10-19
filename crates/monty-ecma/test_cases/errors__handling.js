// === Error construction ===
const e = new Error('plain');
console.log(e.message, e.name, e instanceof Error, String(e), e.toString(), Object.keys(e));
const t = new TypeError('typed');
console.log(t.name, t instanceof TypeError, t instanceof Error, t.constructor === TypeError, `${t}`);
console.log(new RangeError('r').name, new ReferenceError('f').name, new SyntaxError('s').name);
console.log(new EvalError('e').name, new URIError('u').name, Error('no new').message);
console.log(new Error().message === '', new Error(undefined).message, new Error(42).message);
const withCause = new Error('outer', { cause: 'inner' });
console.log(withCause.cause, 'cause' in new Error('x'));
const agg = new AggregateError([new Error('a'), new Error('b')], 'many');
console.log(agg.name, agg.message, agg.errors.length, agg.errors[1].message);

// === Throwing non-errors ===
for (const value of [42, 'str', null, undefined, { k: 1 }, [1, 2]]) {
  try {
    throw value;
  } catch (caught) {
    console.log(typeof caught, caught);
  }
}

// === Catch without binding and rethrow ===
function rethrow() {
  try {
    throw new Error('first');
  } catch {
    throw new Error('second');
  }
}
try {
  rethrow();
} catch (err) {
  console.log(err.message);
}

// === Errors from builtins ===
const attempts = [
  () => null.x,
  () => undefined[0],
  () => (void 0).call(),
  () => ({}).nope(),
  () => notDefined,
  () => new Array(-1),
  () => 'x'.repeat(-1),
  () => (1).toFixed(101),
  () => JSON.parse('{bad'),
  () => JSON.parse(''),
  () => Symbol() + '',
  () => BigInt,
  () => {
    const c = 1;
    c = 2;
  },
  () => null instanceof 5,
  () => 'a' in 'b',
  () => [].reduce((a, b) => a + b),
  () => ({}).x.y,
];
for (const attempt of attempts) {
  try {
    attempt();
    console.log('no error');
  } catch (err) {
    console.log(err.constructor.name + ': ' + err.message);
  }
}

// === Finally sees the exception path ===
function finallyOverridesReturn() {
  try {
    throw new Error('lost');
  } finally {
    return 'finally wins';
  }
}
console.log(finallyOverridesReturn());

// === Error.prototype.toString edge cases ===
const custom = new Error('msg');
custom.name = 'Custom';
console.log(String(custom));
custom.name = '';
console.log(String(custom));
custom.message = '';
console.log(String(custom), String(Object.assign(new Error(), { name: 'OnlyName' })));

// === Uncaught at the end ===
throw new TypeError('uncaught at top level');
