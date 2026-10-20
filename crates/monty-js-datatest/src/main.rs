//! Differential tests for the JavaScript engine: every `crates/monty-ecma/test_cases/*.js`
//! (or `.ts`, `.jsx`, `.tsx`) file runs through `monty-ecma` and through Node, and
//! the two must print the same text and end the same way (an uncaught
//! `Name: message`, or not).
//!
//! The extension picks the dialect on both sides: the engine infers it from the
//! script name, Node strips `.ts` types itself (`--input-type=module-typescript`),
//! and a `.jsx` / `.tsx` case is lowered for Node with oxc's classic JSX transform
//! and run with the reference `jsx` / `renderToString` in `jsx_runtime.js`, so the
//! engine's natives are checked against that JavaScript.
//!
//! Cases configure themselves with a `// /// monty` header (see `monty-fixture`).
//! Run with `make test-js-cases`; `TEST_CASE=<substring>` filters as `cargo test` does.

use std::{
    error::Error,
    fmt, fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use monty_ecma::{JsProgress, JsRepl, JsSuspensionKind};
use monty_fixture::Header;
use monty_types::{
    CollectedStreams, ExcType, ExtFunctionResult, MontyException, NameLookupResult, NamedValues, PrintStream,
    PrintWriter, ResourceLimits,
};
use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer};
use similar::TextDiff;

const TEST_CASES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../monty-ecma/test_cases");

/// The reference JSX runtime prepended to a lowered `.jsx` / `.tsx` case for Node.
const JSX_RUNTIME: &str = include_str!("../jsx_runtime.js");

/// How long Node may take on one case before it is killed and the case fails.
const NODE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a run of a case produced: its stdout and how it ended.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    stdout: String,
    /// `Name: message` of an uncaught error, or `None` when the script completed.
    error: Option<String>,
    /// Heap entries the feed left unreachable (only with `ref-count-return`).
    leaks: Vec<String>,
}

/// A difference between the two runs, rendered as a line diff.
struct Mismatch {
    kind: &'static str,
    node: String,
    monty: String,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} differs from Node (-node +monty):", self.kind)?;
        for change in TextDiff::from_lines(&self.node, &self.monty).iter_all_changes() {
            write!(f, "{}{}", change.tag(), change)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for Mismatch {}

/// Runs one case on both engines and compares.
fn run_case(path: &Path) -> Result<(), Box<dyn Error>> {
    let code = fs::read_to_string(path)?;
    let header = Header::parse_with_prefix(&code, "//")?;
    assert!(
        header.xfail.is_empty(),
        "xfail is not allowed in test cases: fix the behaviour instead"
    );
    let monty = run_monty(path, &code);
    let node = run_node(path, &code)?;
    if monty.stdout != node.stdout {
        return Err(Box::new(Mismatch {
            kind: "stdout",
            node: node.stdout,
            monty: monty.stdout,
        }));
    }
    if monty.error != node.error {
        return Err(Box::new(Mismatch {
            kind: "uncaught error",
            node: node.error.unwrap_or_else(|| "<completed>".to_owned()),
            monty: monty.error.unwrap_or_else(|| "<completed>".to_owned()),
        }));
    }
    if !monty.leaks.is_empty() {
        return Err(format!(
            "the feed leaked {} heap entries: {}",
            monty.leaks.len(),
            monty.leaks.join(", ")
        )
        .into());
    }
    Ok(())
}

/// Runs the case in-process through `monty-ecma`.
fn run_monty(path: &Path, code: &str) -> Outcome {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("case.js");
    let repl = JsRepl::new(name, ResourceLimits::default()).expect("session");
    let mut streams = CollectedStreams::default();
    let mut progress = repl.feed(code, &NamedValues::new(), PrintWriter::collect_streams(&mut streams));
    // there is no host: an unknown name is unknown, as it is in Node
    let (error, mut repl) = loop {
        progress = match progress {
            JsProgress::Complete { repl, .. } => break (None, repl),
            JsProgress::Failed { error, repl } => break (Some(error.summary()), repl),
            JsProgress::Suspended(mut suspension) => match suspension.kind() {
                JsSuspensionKind::FunctionCall { function_name, .. } => {
                    let name = function_name.to_owned();
                    suspension.resume_call(
                        ExtFunctionResult::NotFound(name),
                        PrintWriter::collect_streams(&mut streams),
                    )
                }
                JsSuspensionKind::NameLookup { .. } => suspension
                    .resume_name_lookup(NameLookupResult::Undefined, PrintWriter::collect_streams(&mut streams)),
                JsSuspensionKind::OsCall { .. } => {
                    let exc = MontyException::new(ExcType::Error, Some("no host services OS calls".to_owned()));
                    suspension.abort(&exc, PrintWriter::collect_streams(&mut streams))
                }
                JsSuspensionKind::ResolveFutures { .. } => {
                    let exc = MontyException::new(ExcType::Error, Some("no host to resolve futures".to_owned()));
                    suspension.abort(&exc, PrintWriter::collect_streams(&mut streams))
                }
            },
        };
    };
    // only stdout is compared: Node's stderr carries its own diagnostics
    let stdout = streams
        .into_entries()
        .into_iter()
        .filter(|(stream, _)| *stream == PrintStream::Stdout)
        .map(|(_, text)| text)
        .collect();
    Outcome {
        stdout,
        error,
        leaks: leaked_entries(&mut repl),
    }
}

/// Heap entries the feed left unreachable, when the leak detector is compiled in.
#[cfg(feature = "ref-count-return")]
fn leaked_entries(repl: &mut JsRepl) -> Vec<String> {
    repl.leaked_entries()
}

#[cfg(not(feature = "ref-count-return"))]
fn leaked_entries(_: &mut JsRepl) -> Vec<String> {
    Vec::new()
}

/// Runs the case under `node` as an ES module (strict mode and a module
/// `this`, like the engine), killing it after [`NODE_TIMEOUT`]. TypeScript is
/// stripped by Node itself; JSX is lowered here first.
fn run_node(path: &Path, code: &str) -> Result<Outcome, Box<dyn Error>> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("case.js");
    let extension = name.rsplit_once('.').map_or("js", |(_, ext)| ext);
    let (input_type, code) = match extension {
        "ts" => ("--input-type=module-typescript", code.to_owned()),
        "jsx" | "tsx" => (
            "--input-type=module",
            format!("{JSX_RUNTIME}\n{}", lower_jsx(name, code)?),
        ),
        _ => ("--input-type=module", code.to_owned()),
    };
    let mut child = Command::new("node")
        .args(["--no-warnings", input_type, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("TZ", "UTC")
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot run node (is it installed?): {err}"))?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(code.as_bytes())?;
    }
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout_pipe.read_to_string(&mut text);
        text
    });
    let stderr_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr_pipe.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + NODE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("node did not finish within {NODE_TIMEOUT:?}").into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_reader.join().expect("stdout reader");
    let stderr = stderr_reader.join().expect("stderr reader");
    let error = if status.success() {
        None
    } else {
        Some(node_error_summary(&stderr))
    };
    Ok(Outcome {
        stdout,
        error,
        leaks: Vec::new(),
    })
}

/// Lowers a JSX case to the JavaScript Node runs: types erased, every element
/// a `jsx(type, props, ...children)` call (`Fragment` for `<>`), as the engine
/// does internally with the same transformer.
fn lower_jsx(name: &str, code: &str) -> Result<String, Box<dyn Error>> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(name).map_err(|err| err.to_string())?;
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if let Some(error) = parsed.diagnostics.errors().next() {
        return Err(format!("oxc cannot parse the case for Node: {}", error.message).into());
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
    let mut options = TransformOptions::default();
    options.typescript.jsx_pragma = "jsx".into();
    options.typescript.jsx_pragma_frag = "Fragment".into();
    options.jsx = JsxOptions {
        runtime: JsxRuntime::Classic,
        pragma: Some("jsx".to_owned()),
        pragma_frag: Some("Fragment".to_owned()),
        pure: false,
        display_name_plugin: false,
        throw_if_namespace: false,
        ..JsxOptions::enable()
    };
    let transformed = Transformer::new(&allocator, Path::new(name), &options).build_with_scoping(scoping, &mut program);
    if let Some(error) = transformed.diagnostics.errors().next() {
        return Err(format!("oxc cannot lower the case for Node: {}", error.message).into());
    }
    Ok(Codegen::new().build(&program).code)
}

/// The `Name: message` line of Node's uncaught-error report: the first line
/// that names an error class, else the whole report. Node's own error codes
/// (`SyntaxError [ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX]: …`) are dropped, since
/// the engine reports the class alone.
fn node_error_summary(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim_end)
        .find_map(|line| {
            let (name, message) = line.split_once(": ").unwrap_or((line, ""));
            let (class, code) = name.split_once(" [").unwrap_or((name, ""));
            let coded = code.is_empty() || code.ends_with(']');
            let named = !class.is_empty()
                && class.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && (class.ends_with("Error") || line == "Uncaught");
            (coded && named).then(|| {
                if code.is_empty() {
                    line.to_owned()
                } else {
                    format!("{class}: {message}")
                }
            })
        })
        .unwrap_or_else(|| stderr.trim().to_owned())
}

datatest_stable::harness!(run_case, TEST_CASES_DIR, r"^.*\.(js|jsx|ts|tsx)$");
