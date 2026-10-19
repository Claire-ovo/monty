//! The guards that keep hostile shapes from overflowing the interpreter's
//! own stack: deeply nested source, structures walked by builtins, and
//! builtins calling each other.

use std::{fmt::Write, thread};

use monty_ecma::{JsProgress, JsRepl};
use monty_types::{NamedValues, PrintWriter, ResourceLimits};

/// The stack the worker runs the interpreter on (`monty_proto::worker::stdio`);
/// the guards are tuned for it, not for a test thread's 2 MiB.
const WORKER_STACK: usize = 16 * 1024 * 1024;

/// The uncaught error summary, or `None` when the feed completed.
fn outcome(code: &str) -> Option<String> {
    let code = code.to_owned();
    thread::Builder::new()
        .stack_size(WORKER_STACK)
        .spawn(move || {
            let repl = JsRepl::new("main.js", ResourceLimits::default()).expect("session");
            match repl.feed(&code, &NamedValues::new(), PrintWriter::Disabled) {
                JsProgress::Complete { .. } => None,
                JsProgress::Failed { error, .. } => Some(error.summary()),
                JsProgress::Suspended(_) => Some("suspended".to_owned()),
            }
        })
        .expect("spawn")
        .join()
        .expect("the feed must not abort")
}

#[test]
fn deeply_nested_source_is_a_syntax_error() {
    let too_deep = Some("SyntaxError: Source is too deeply nested".to_owned());
    let n = 2000;
    assert_eq!(outcome(&format!("{}1{}", "(".repeat(n), ")".repeat(n))), too_deep);
    assert_eq!(outcome(&format!("{}1{}", "[".repeat(n), "]".repeat(n))), too_deep);
    assert_eq!(outcome(&format!("{}1{}", "{a:".repeat(n), "}".repeat(n))), too_deep);
    assert_eq!(outcome(&format!("{}1{}", "(() => ".repeat(n), ")".repeat(n))), too_deep);
    assert_eq!(outcome(&format!("{}1", "!".repeat(n))), too_deep);
    assert_eq!(outcome(&format!("{}1", "1 + ".repeat(n))), too_deep);
    assert_eq!(outcome(&format!("{}2", "a ? 1 : ".repeat(n))), too_deep);
    assert_eq!(
        outcome(&format!("let x = 0; {}{{}}", "if (x) {} else if (x) ".repeat(n))),
        too_deep
    );
    assert_eq!(outcome(&format!("{}1{}", "`${".repeat(n), "}`".repeat(n))), too_deep);
    assert_eq!(outcome(&format!("let o = {{a: 1}}; o{}", ".a".repeat(n))), too_deep);
    // the same pre-check serves hosts before a feed
    let repl = JsRepl::new("main.js", ResourceLimits::default()).expect("session");
    assert!(repl.check_source(&"(".repeat(n)).is_err());
    assert!(repl.check_source("(1)").is_ok());
}

#[test]
fn ordinary_breadth_is_not_nesting() {
    let items = (0..3000).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    assert_eq!(outcome(&format!("[{items}].length")), None);
    let statements = (0..1000).fold(String::new(), |mut out, i| {
        let _ = write!(out, "let v{i} = {i};");
        out
    });
    assert_eq!(outcome(&statements), None);
    let functions = (0..500).fold(String::new(), |mut out, i| {
        let _ = writeln!(out, "function f{i}() {{ return {i}; }}");
        out
    });
    assert_eq!(outcome(&format!("{functions}f499()")), None);
    let chain = (0..60).map(|i| i.to_string()).collect::<Vec<_>>().join(" + ");
    assert_eq!(outcome(&format!("const total = {chain};\ntotal")), None);
    let branches = (0..40).fold(String::new(), |mut out, i| {
        let _ = write!(out, "else if (x === {i}) {{ y = {i}; }} ");
        out
    });
    assert_eq!(
        outcome(&format!(
            "let x = 5, y = 0; if (x < 0) {{ y = -1; }} {branches}else {{ y = 99; }}\ny"
        )),
        None
    );
    assert_eq!(
        outcome("const text = `a ${'b'} c ${[1, 2].map((n) => `${n}`).join('')} // not a comment`; /[(]/.test('(')"),
        None
    );
    assert_eq!(outcome(&format!("'{}'.length", "(".repeat(5000))), None);
}

#[test]
fn deep_structures_are_rejected_not_overflowed() {
    let stack = Some("RangeError: Maximum call stack size exceeded".to_owned());
    assert_eq!(
        outcome("let o = {}; for (let i = 0; i < 100000; i++) o = { o }; JSON.stringify(o)"),
        stack
    );
    assert_eq!(
        outcome("let a = []; for (let i = 0; i < 100000; i++) a = [a]; String(a)"),
        stack
    );
    assert_eq!(
        outcome("let a = []; for (let i = 0; i < 100000; i++) a = [a]; a.join()"),
        stack
    );
    assert_eq!(outcome("const o = {}; o.toString = () => o + ''; String(o)"), stack);
    assert_eq!(outcome("const f = () => [1].map(f); f()"), stack);
    assert_eq!(outcome("function f() { f(); } f()"), stack);
    assert_eq!(
        outcome("let o = {}; for (let i = 0; i < 100000; i++) o = { o }; o"),
        Some("RangeError: value is nested too deeply to send to the host".to_owned())
    );
    // flattening walks with its own stack
    assert_eq!(
        outcome("let a = [1]; for (let i = 0; i < 100000; i++) a = [a]; a.flat(Infinity).length === 1 ? 0 : fail()"),
        None
    );
    assert_eq!(outcome("JSON.parse('['.repeat(100000) + ']'.repeat(100000))"), stack);
}
