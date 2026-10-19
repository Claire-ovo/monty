// === Primitives ===
console.log('plain string', 42, -0, 1.5, true, null, undefined);
console.log(1e21, 1e-7, 123456789012345680000, 0.1 + 0.2, NaN, Infinity, -Infinity);
console.log(Symbol('s'), Symbol());

// === Nested strings are quoted ===
console.log(['a', "it's", 'say "hi"', 'both \' and "', 'tab\there', 'new\nline']);

// === Objects and keys ===
console.log({ a: 1, 'b-c': 2, 3: 'three', $d: 4, _e: 5, 'spaced key': 6 });
console.log({}, [], [[]], [{}], { nested: { deeper: { deepest: { gone: 1 } } } });
console.log([1, [2, [3, [4, [5]]]]]);

// === Long arrays break into columns ===
console.log([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]);
console.log(['alpha', 'beta', 'gamma', 'delta', 'epsilon', 'zeta', 'eta', 'theta', 'iota']);
const big = [];
for (let i = 0; i < 120; i++) big.push(i);
console.log(big);

// === Long objects break across lines ===
console.log({
  firstName: 'Ada',
  lastName: 'Lovelace',
  born: 1815,
  fields: ['mathematics', 'computing'],
  note: 'first programmer',
});

// === Functions and holes ===
function named() {}
console.log(named, () => {}, function () {}, Math.max, [, 1, , , 2]);

// === Getters, boxed primitives and errors in containers ===
console.log({ get g() { return 1; }, set s(v) {} });
console.log([new Number(3), new String('x'), new Boolean(false)]);

// === Format directives ===
console.log('%s and %d and %i and %f', 'str', 42.5, 42.5, 1.25, 'extra');
console.log('%j %o %%', { a: [1] }, [2]);
console.log('no args %s');

// === Multiple streams and empty calls ===
console.log();
console.error('to stderr');
console.info('info goes to stdout');
console.warn('warn goes to stderr');
console.debug('debug goes to stdout');
