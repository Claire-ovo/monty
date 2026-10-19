//! Differential tests for the JavaScript engine: every `crates/monty-ecma/test_cases/*.js`
//! file runs through `monty-ecma` and through Node, and the two must print the
//! same text and end the same way (an uncaught `Name: message`, or not).
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

use monty_ecma::{JsProgress, JsRepl};
use monty_fixture::Header;
use monty_types::{CollectedStreams, NamedValues, PrintStream, PrintWriter, ResourceLimits};
use similar::TextDiff;

const TEST_CASES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../monty-ecma/test_cases");

/// How long Node may take on one case before it is killed and the case fails.
const NODE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a run of a case produced: its stdout and how it ended.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    stdout: String,
    /// `Name: message` of an uncaught error, or `None` when the script completed.
    error: Option<String>,
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
    let node = run_node(&code)?;
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
    Ok(())
}

/// Runs the case in-process through `monty-ecma`.
fn run_monty(path: &Path, code: &str) -> Outcome {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("case.js");
    let repl = JsRepl::new(name, ResourceLimits::default()).expect("session");
    let mut streams = CollectedStreams::default();
    let error = match repl.feed(code, &NamedValues::new(), PrintWriter::collect_streams(&mut streams)) {
        JsProgress::Complete { .. } => None,
        JsProgress::Failed { error, .. } => Some(error.summary()),
    };
    // only stdout is compared: Node's stderr carries its own diagnostics
    let stdout = streams
        .into_entries()
        .into_iter()
        .filter(|(stream, _)| *stream == PrintStream::Stdout)
        .map(|(_, text)| text)
        .collect();
    Outcome { stdout, error }
}

/// Runs the case under `node` as an ES module (strict mode and a module
/// `this`, like the engine), killing it after [`NODE_TIMEOUT`].
fn run_node(code: &str) -> Result<Outcome, Box<dyn Error>> {
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
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
    Ok(Outcome { stdout, error })
}

/// The `Name: message` line of Node's uncaught-error report: the first line
/// that names an error class, else the whole report.
fn node_error_summary(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim_end)
        .find(|line| {
            let (name, _) = line.split_once(": ").unwrap_or((line, ""));
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && (name.ends_with("Error") || *line == "Uncaught")
        })
        .map_or_else(|| stderr.trim().to_owned(), ToOwned::to_owned)
}

datatest_stable::harness!(run_case, TEST_CASES_DIR, r"^.*\.js$");
