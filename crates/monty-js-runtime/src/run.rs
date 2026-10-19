//! The standalone runner: `monty-js file.js` / `monty-js -e code`, which
//! behaves like `node`: `console.log` goes to stdout, an uncaught error is
//! reported on stderr and exits with status 1.

use std::{fs, process::ExitCode, time::Duration};

use monty_ecma::{JsProgress, JsRepl};
use monty_types::{Language, NamedValues, PrintWriter, ResourceLimits};

use crate::Cli;

pub(crate) fn run(cli: &Cli) -> ExitCode {
    let (name, code) = match (&cli.eval, &cli.file) {
        (Some(code), _) => ("[eval]".to_owned(), code.clone()),
        (None, Some(path)) => match fs::read_to_string(path) {
            Ok(code) => (path.clone(), code),
            Err(err) => {
                eprintln!("error: cannot read {path}: {err}");
                return ExitCode::FAILURE;
            }
        },
        (None, None) => {
            eprintln!("error: pass a file to run or `-e <code>`");
            return ExitCode::FAILURE;
        }
    };
    let language = cli.language.unwrap_or_else(|| {
        cli.file
            .as_deref()
            .and_then(Language::from_extension)
            .unwrap_or(Language::JavaScript)
    });
    if language != Language::JavaScript {
        // TODO(ts-syntax): TypeScript feeds go through the same engine once it accepts them.
        eprintln!("error: {language} is not supported yet");
        return ExitCode::FAILURE;
    }
    let limits = match resource_limits(cli) {
        Ok(limits) => limits,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let repl = match JsRepl::new(&name, limits) {
        Ok(repl) => repl,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    match repl.feed(&code, &NamedValues::new(), PrintWriter::Stdout) {
        JsProgress::Complete { .. } => ExitCode::SUCCESS,
        JsProgress::Failed { error, .. } => {
            for frame in error.traceback() {
                eprint!("{frame}");
            }
            eprintln!("Uncaught {}", error.summary());
            ExitCode::FAILURE
        }
    }
}

/// Builds the limits from the flags; the defaults otherwise.
fn resource_limits(cli: &Cli) -> Result<ResourceLimits, String> {
    let mut limits = ResourceLimits::default();
    if let Some(bytes) = cli.max_memory {
        limits = limits.max_memory(bytes);
    }
    if let Some(secs) = cli.max_feed_duration {
        let duration =
            Duration::try_from_secs_f64(secs).map_err(|err| format!("invalid --max-feed-duration: {err}"))?;
        limits = limits.max_feed_duration(duration);
    }
    if let Some(depth) = cli.max_recursion_depth {
        limits = limits.max_recursion_depth(depth);
    }
    Ok(limits)
}
