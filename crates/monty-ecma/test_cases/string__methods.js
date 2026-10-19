// === Basics ===
const s = 'Hello, World';
console.log(s.length, s[0], s[100], s.charAt(4), s.charAt(100), s.charCodeAt(0), s.charCodeAt(100), s.codePointAt(0));
console.log(s.at(-1), s.at(0), s.at(100), 'é'.length, '😀'.length, '😀'.codePointAt(0), '😀'.charCodeAt(1), [...'a😀b'].length);

// === Searching ===
console.log(s.indexOf('o'), s.indexOf('o', 5), s.indexOf('z'), s.lastIndexOf('o'), s.lastIndexOf('o', 5), s.indexOf(''));
console.log(s.includes('World'), s.includes('world'), s.startsWith('Hell'), s.startsWith('ell', 1), s.endsWith('ld'), s.endsWith('Hello', 5));

// === Slicing ===
console.log(s.slice(7), s.slice(-5), s.slice(2, 4), s.slice(4, 2), s.substring(2, 4), s.substring(4, 2), s.substring(-3, 2));
console.log(s.split(', '), s.split(''), s.split(), s.split('l', 2), 'a,b,,c'.split(','), ''.split(','), ''.split(''));

// === Case and whitespace ===
console.log(s.toUpperCase(), s.toLowerCase(), 'ß'.toUpperCase(), '  pad  '.trim() + '|', '  pad  '.trimStart() + '|', '  pad  '.trimEnd() + '|');
console.log('\t\n x  '.trim().length, 'İ'.toLowerCase().length);

// === Building ===
console.log('ab'.repeat(3), 'ab'.repeat(0), '5'.padStart(3, '0'), '5'.padEnd(3), 'abc'.padStart(6, '12'), 'abc'.padStart(2, 'x'));
console.log('a'.concat('b', 1, null), 'x' + 1 + 2, 1 + 2 + 'x', `${'nested ' + `${1 + 1}`}`);

// === Replacing ===
console.log('aaa'.replace('a', 'b'), 'aaa'.replaceAll('a', 'b'), 'abc'.replace('b', '[$&]'), 'abc'.replace('b', "$`|$'"), 'a$b'.replace('$', '$$'));
console.log('a-b-c'.replace('-', (m, i) => `(${m}@${i})`), 'xyz'.replace('', '^'), 'aaa'.replaceAll('', '.'));

// === Comparison ===
console.log('a' < 'b', 'a' < 'B', 'abc'.localeCompare('abd'), 'b'.localeCompare('a'), 'a'.localeCompare('a'));

// === Conversions ===
console.log(String.fromCharCode(72, 105), String.fromCharCode(0x1f600), 'A'.codePointAt(), String(Symbol('d')), new String('boxed').length);
console.log(typeof new String('x'), new String('x') == 'x', new String('x') === 'x', 'x'.toString(), 'x'.valueOf());
console.log('abc'.split('').reverse().join(''), [...'abc'].map((c) => c.charCodeAt(0)));

// === Iteration ===
for (const ch of 'hi') console.log(ch);
console.log([...'ab'], Array.from('ab').length);

// === Errors ===
for (const attempt of [() => 'x'.repeat(-1), () => 'x'.repeat(Infinity), () => null.length, () => 'a'.foo()]) {
  try {
    attempt();
  } catch (err) {
    console.log(err.constructor.name, err.message);
  }
}

// === normalize ===
const composed = 'é';
const decomposed = 'é';
console.log(composed === decomposed, composed.normalize() === decomposed.normalize(), decomposed.normalize('NFC').length, composed.normalize('NFD').length, 'ﬁ'.normalize('NFKC'), 'ﬁ'.normalize('NFKD'), 'ﬁ'.normalize(), 'abc'.normalize('NFKD'));
console.log([...'한글'.normalize('NFD')].length, '한글'.normalize('NFD').normalize('NFC') === '한글', 'ẛ̣'.normalize('NFKC').length, ''.normalize());
try {
  'a'.normalize('nfc');
} catch (err) {
  console.log(err.constructor.name + ': ' + err.message);
}
