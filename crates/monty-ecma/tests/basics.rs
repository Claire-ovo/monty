//! End-to-end checks of the JavaScript engine: feed a snippet, compare what
//! it printed and returned.

use insta::assert_snapshot;
use monty_ecma::{JsProgress, JsRepl};
use monty_types::{MontyObject, NamedValues, PrintWriter, ResourceLimits};

/// Runs `code` in a fresh session: the printed text, then the completion
/// value or the uncaught error summary.
fn run(code: &str) -> String {
    let repl = JsRepl::new("main.js", ResourceLimits::default()).expect("session");
    let mut output = String::new();
    let progress = repl.feed(code, &NamedValues::new(), PrintWriter::collect_string(&mut output));
    let tail = match progress {
        JsProgress::Complete { value, .. } => format!("=> {}", value.py_repr()),
        JsProgress::Failed { error, .. } => format!("!! {}", error.summary()),
    };
    format!("{output}{tail}")
}

/// The completion value alone.
fn eval(code: &str) -> MontyObject {
    let repl = JsRepl::new("main.js", ResourceLimits::default()).expect("session");
    match repl.feed(code, &NamedValues::new(), PrintWriter::Disabled) {
        JsProgress::Complete { value, .. } => value,
        JsProgress::Failed { error, .. } => panic!("unexpected error: {}", error.summary()),
    }
}

#[test]
fn arithmetic_and_strings() {
    assert_eq!(eval("1 + 2 * 3"), MontyObject::int(7));
    assert_eq!(eval("'a' + 1 + 2"), MontyObject::string("a12"));
    assert_eq!(eval("7 % 3 + 2 ** 3"), MontyObject::int(9));
    assert_eq!(eval("0.1 + 0.2"), MontyObject::float(0.30000000000000004));
    assert_eq!(eval("'abc'.length"), MontyObject::int(3));
    assert_eq!(eval("`x${1 + 1}y`"), MontyObject::string("x2y"));
}

#[test]
fn console_log_formatting() {
    assert_snapshot!(run(r#"
console.log('hello', 1, true, null, undefined);
console.log([1, 'two', [3]], { a: 1, b: 'x', c: { d: [] } });
console.log(-0, 1e21, 1e-7, 0.5, NaN);
console.log(function f() {}, () => 1, Math.max);
console.log([1, 2, 3, 4, 5, 6, 7]);
console.log('%s is %d years', 'Bob', 42);
"#), @"");
}

#[test]
fn closures_and_scopes() {
    assert_snapshot!(run(r#"
function counter() {
  let n = 0;
  return () => ++n;
}
const c = counter();
c(); c();
console.log(c());

const fns = [];
for (let i = 0; i < 3; i++) {
  fns.push(() => i);
}
console.log(fns.map(f => f()));

var v = 1;
{
  let v = 2;
  console.log(v);
}
console.log(v);
"#), @"");
}

#[test]
fn exceptions() {
    assert_snapshot!(run(r#"
function f() {
  try {
    throw new TypeError('bad');
  } catch (e) {
    console.log(e instanceof TypeError, e.message);
  } finally {
    console.log('finally');
  }
}
f();
try {
  null.x;
} catch (e) {
  console.log(e.constructor === TypeError, e.message);
}
undefinedVariable;
"#), @"");
}

#[test]
fn tdz_and_const() {
    assert_snapshot!(run("{ console.log(x); let x = 1; }"), @"");
    assert_snapshot!(run("const k = 1; k = 2;"), @"");
}

#[test]
fn objects_and_arrays() {
    assert_snapshot!(run(r#"
const o = { a: 1, 'b c': 2, [1 + 1]: 3, get g() { return 'got'; }, m() { return this.a; } };
console.log(o, o.g, o.m(), Object.keys(o));
const arr = [3, 1, 2];
arr.sort();
console.log(arr, arr.includes(2), arr.indexOf(3), [1, [2, [3, [4]]]].flat(2));
console.log(JSON.stringify({ x: [1, { y: null }], s: 'q"' }), JSON.parse('{"a":[1,2,{"b":true}]}'));
for (const k in { p: 1, q: 2 }) console.log(k);
for (const ch of 'hé') console.log(ch);
console.log([...'ab', ...[1, 2]], { ...o, z: 9 }.z);
"#), @"");
}

#[test]
fn session_persists_bindings() {
    let repl = JsRepl::new("main.js", ResourceLimits::default()).expect("session");
    let JsProgress::Complete { repl, .. } =
        repl.feed("let total = 1; function add(n) { total += n; }", &NamedValues::new(), PrintWriter::Disabled)
    else {
        panic!("first feed failed");
    };
    let JsProgress::Complete { value, .. } = repl.feed("add(41); total", &NamedValues::new(), PrintWriter::Disabled)
    else {
        panic!("second feed failed");
    };
    assert_eq!(value, MontyObject::int(42));
}

#[test]
fn syntax_error() {
    assert_snapshot!(run("let x = ;"), @"");
    assert_snapshot!(run("with (a) {}"), @"");
}

#[test]
fn inputs_are_globals() {
    let repl = JsRepl::new("main.js", ResourceLimits::default()).expect("session");
    let mut inputs = NamedValues::new();
    inputs.push("data", MontyObject::list([MontyObject::int(1), MontyObject::string("x")]));
    let JsProgress::Complete { value, .. } =
        repl.feed("data.length + data[0]", &inputs, PrintWriter::Disabled)
    else {
        panic!("feed failed");
    };
    assert_eq!(value, MontyObject::int(3));
}
