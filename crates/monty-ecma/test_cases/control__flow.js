// === if / else chains ===
function sign(n) {
  if (n > 0) return 'positive';
  else if (n < 0) return 'negative';
  else return 'zero';
}
console.log(sign(3), sign(-1), sign(0));

// === while / do-while ===
let i = 0;
while (i < 3) i++;
console.log(i);
do {
  i--;
} while (i > 0);
console.log(i);

// === for with break and continue ===
const out = [];
for (let k = 0; k < 10; k++) {
  if (k % 2) continue;
  if (k > 6) break;
  out.push(k);
}
console.log(out);

// === Labelled loops ===
const pairs = [];
outer: for (let a = 0; a < 3; a++) {
  for (let b = 0; b < 3; b++) {
    if (b === 2) continue outer;
    if (a === 2) break outer;
    pairs.push([a, b]);
  }
}
console.log(pairs);

// === switch with fallthrough and default ===
function describe(x) {
  switch (x) {
    case 1:
    case 2:
      return 'small';
    case 'a':
      return 'letter';
    default:
      return 'other';
  }
}
console.log(describe(1), describe(2), describe('a'), describe(null));

let fell = '';
switch (2) {
  case 1:
    fell += '1';
  case 2:
    fell += '2';
  case 3:
    fell += '3';
    break;
  case 4:
    fell += '4';
}
console.log(fell);

// === try / catch / finally ordering ===
function order() {
  const log = [];
  try {
    log.push('try');
    throw new Error('boom');
  } catch (e) {
    log.push('catch ' + e.message);
  } finally {
    log.push('finally');
  }
  return log;
}
console.log(order());

function returnThroughFinally() {
  try {
    return 'from try';
  } finally {
    console.log('finally runs before return');
  }
}
console.log(returnThroughFinally());

function breakThroughFinally() {
  const log = [];
  for (const x of [1, 2, 3]) {
    try {
      if (x === 2) break;
      log.push(x);
    } finally {
      log.push('f' + x);
    }
  }
  return log;
}
console.log(breakThroughFinally());

function nestedFinally() {
  const log = [];
  try {
    try {
      throw new Error('inner');
    } finally {
      log.push('inner finally');
    }
  } catch (e) {
    log.push('outer catch ' + e.message);
  }
  return log;
}
console.log(nestedFinally());

// === for-of over arrays, strings and for-in over objects ===
for (const pair of [['a', 1]]) {
  console.log(pair[0], pair[1]);
}
let chars = '';
for (const ch of 'héllo') chars += ch.toUpperCase();
console.log(chars);
const keys = [];
for (const key in { x: 1, y: 2 }) keys.push(key);
console.log(keys);

// === Comma, conditional and logical short-circuits ===
let evaluated = 0;
const value = (evaluated++, false) || (evaluated++, 0) || (evaluated++, 'third');
console.log(value, evaluated);
console.log(null ?? 'default', 0 ?? 'default', undefined?.x, null?.a.b.c);
