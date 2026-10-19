// === Literals, properties and inspection ===
const re = /(\d+)-(?<word>[a-z]+)/gi;
console.log(re, re.source, re.flags, re.global, re.ignoreCase, re.multiline, re.sticky, re.lastIndex);
console.log(String(re), typeof re, re instanceof RegExp, Object.prototype.toString.call(re), re.constructor === RegExp);
console.log(/a\/b/, new RegExp('a/b'), new RegExp(''), /x/.source, new RegExp('\n').source, /[/]/.source);
console.log(Object.keys(re), Object.getOwnPropertyNames(re), Object.getOwnPropertyDescriptor(re, 'lastIndex'));
console.log(new RegExp('a+', 'g'), new RegExp(/x/i), new RegExp(/x/i, 'gm'), new RegExp('\\d'), RegExp(/y/) , RegExp('z', 'y'));
const original = /q/g;
console.log(RegExp(original) === original, new RegExp(original) === original, { nested: { deep: { re: /deep/ } } });
console.log(RegExp.prototype.source, RegExp.prototype.flags, RegExp.prototype.global, String(RegExp.prototype));
console.log(/a/dgimsuy.flags, /a/v.flags, /a/s.dotAll, /a/u.unicode, /a/v.unicodeSets, /a/d.hasIndices, RegExp.prototype.toString.length);

// === exec, test and lastIndex ===
const input = '12-ab 34-CD';
console.log(re.exec(input), re.lastIndex);
console.log(re.exec(input).groups, re.lastIndex, re.exec(input), re.lastIndex, re.exec(input), re.lastIndex);
console.log(/b/.exec('abc'), /z/.exec('abc'), /(y)?/.exec('x'), /(?<a>x)|(?<b>y)/.exec('y').groups);
console.log(/a/.test('cat'), /^a/.test('cat'), /A/i.test('cat'), /^.$/.test('\n'), /^.$/s.test('\n'), /^b/m.test('a\nb'));
const sticky = /o/y;
console.log(sticky.test('foo'), sticky.lastIndex, (sticky.lastIndex = 1), sticky.test('foo'), sticky.lastIndex, sticky.test('foo'), sticky.lastIndex);
const global = /a/g;
global.lastIndex = 99;
console.log(global.exec('aaa'), global.lastIndex, global.test('aaa'), global.lastIndex);
console.log(/\u{1F600}/u.test('😀'), /^.$/u.test('😀'), /^.$/.test('😀'), '😀'.match(/./gu), '😀'.match(/./g).length);
console.log(/(a)(b)?(c)/d.exec('ac'), /(?<n>a)/d.exec('a').indices.groups);

// === String.prototype.match, matchAll and search ===
console.log('abc'.match(/b/), 'abc'.match(/z/), 'a1b2'.match(/\d/g), 'x'.match(/(y)?/), 'abc'.match(), 'abc'.match('b'), 'a.c'.match('.'));
console.log('aaa'.match(/a*/g), ''.match(/x*/g), 'abc'.match(/(?:)/g), 'ab\ncd'.match(/^./gm));
console.log([...'a1b22c333'.matchAll(/\d+/g)].map((m) => [m[0], m.index]), [...'abc'.matchAll('b')], [...'xx'.matchAll(/y/g)]);
const iterator = 'a1'.matchAll(/\d/g);
console.log(iterator, iterator.next(), iterator.next(), iterator.next(), Object.prototype.toString.call(iterator));
const sharedStart = /\d/g;
sharedStart.lastIndex = 2;
console.log([...'1234'.matchAll(sharedStart)].map((m) => m[0]), sharedStart.lastIndex, [...'aaa'.matchAll(/a*/g)].map((m) => m.index));
console.log('hello world'.search(/o/), 'hello'.search(/z/), 'hello'.search('l'), 'hello'.search(), 'a.b'.search('.'));
const searched = /l/g;
searched.lastIndex = 4;
console.log('hello'.search(searched), searched.lastIndex);

// === replace and replaceAll ===
console.log('a-b-c'.replace(/-/g, '+'), 'a-b-c'.replace(/-/, '+'), 'John Smith'.replace(/(\w+)\s(\w+)/, '$2, $1'), 'aaa'.replace(/a/g, (m, i) => i));
console.log('2020-01-02'.replace(/(?<y>\d+)-(?<m>\d+)-(?<d>\d+)/, '$<d>/$<m>/$<y>'), 'abc'.replaceAll(/b/g, 'B'), 'abc'.replace(/(b)/, '[$1$2$$$&$`$\']'));
console.log('abc'.replace(/b/, '$<x>'), 'abc'.replace(/(?<x>b)/, '[$<x>$<y>$<x]'), 'abcdefghijkl'.replace(/(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)(l)/, '$12$1$01$00$13'));
console.log('xAyBz'.replace(/[A-Z]/g, (m, offset, whole) => `${m.toLowerCase()}${offset}${whole.length}`), 'ab'.replace(/(?<first>a)/, (...args) => JSON.stringify(args)));
console.log('aaa'.replace(/a*/g, '-'), 'abc'.replace(/x*/g, '-'), 'abc'.replace(/(?:)/g, '.'), 'aaa'.replace(/(a)|b/g, '[$1]'), 'abc'.replace(/(b)|(z)/, '[$2]'));
console.log('a.b.c'.replaceAll('.', '!'), 'a.b.c'.replace('.', '!'), 'x'.replace(/x/, '$'), 'x'.replace(/x/, '$0'), 'x'.replace(/x/, '$$'));
try {
  'abc'.replaceAll(/b/, 'B');
} catch (err) {
  console.log(err.constructor.name + ': ' + err.message);
}
try {
  'abc'.matchAll(/b/);
} catch (err) {
  console.log(err.constructor.name + ': ' + err.message);
}

// === split ===
console.log('a.b.c'.split(/\./), 'a1b2c'.split(/\d/, 2), 'abc'.split(/(b)/), 'abc'.split(/(?:)/), 'abc'.split(/x*/), ''.split(/x/), ''.split(/x*/));
console.log('a, b,c'.split(/\s*,\s*/), 'one  two'.split(/(\s)+/), 'abc'.split(/b/, 0), 'ab'.split(/(?<l>a)|(?<r>b)/), 'test'.split(/t/i));
console.log('😀x😀'.split(/(?:)/u), '😀x😀'.split(/(?:)/).length, 'AbC'.split(/b/i), 'a\nb'.split(/^/m));

// === The Symbol protocol on custom objects ===
const custom = {
  flags: 'g',
  [Symbol.match](s) {
    return ['custom match ' + s];
  },
  [Symbol.replace](s, r) {
    return `${s}|${r}`;
  },
  [Symbol.search](s) {
    return s.length;
  },
  [Symbol.split](s, limit) {
    return [s, limit];
  },
  [Symbol.matchAll](s) {
    return [s][Symbol.iterator]();
  },
};
console.log('abc'.match(custom), 'abc'.replace(custom, 'r'), 'abc'.search(custom), 'abc'.split(custom, 2), [...'abc'.matchAll(custom)]);
console.log('abc'.replaceAll(custom, 'r'), 'abc'.replaceAll({ [Symbol.match]: true, flags: 'g', [Symbol.replace]: () => 'ok' }, 'r'));

// === Errors ===
const attempts = [
  () => new RegExp('a', 'gg'),
  () => new RegExp('a', 'x'),
  () => RegExp.prototype.exec.call({}, 'a'),
  () => RegExp.prototype.test.call('a'),
  () => Object.getOwnPropertyDescriptor(RegExp.prototype, 'global').get.call({}),
  () => 'abc'.replaceAll({ [Symbol.match]: true, flags: 'i' }, 'r'),
  () => 'abc'.match({ [Symbol.match]: 5 }),
  () => {
    const frozen = Object.freeze(/a/g);
    frozen.exec('a');
  },
];
for (const attempt of attempts) {
  try {
    console.log(attempt());
  } catch (err) {
    console.log(err.constructor.name + ': ' + err.message);
  }
}
try {
  new RegExp('(');
} catch (err) {
  console.log(err instanceof SyntaxError, err.message.startsWith('Invalid regular expression: /(/: '));
}

// === Patterns ===
console.log(/^[\w.+-]+@[\w-]+\.[\w.]+$/.test('a.b+c@example.co.uk'), /(\w)\1/.exec('hello'), /(?<=\$)\d+/.exec('cost $42'), /\d+(?!px)/.exec('10px 20em'));
console.log('The Quick Brown'.replace(/\b(\w)(\w*)/g, (m, first, rest) => first.toLowerCase() + rest.toUpperCase()), /a{2,3}/.exec('aaaa'), /a{2,3}?/.exec('aaaa'));
console.log(/[^\d\s]+/.exec('12 ab'), /\p{Lu}+/u.exec('abcDEF'), /[\p{L}--[a-z]]+/v.exec('abcDEFghi'), /(?i:a)b/.test('Ab'), /(?i:a)b/.test('AB'));
console.log('a\tb c'.split(/\s/), /(a|ab)(c|bcd)(d*)/.exec('abcd'), /x*/.exec('yyy'), /\bfoo\b/.test('a foo b'), /\Bfoo/.test('a foo b'));
