//! The standalone runner: `monty-js file.js` / `monty-js -e code`, which
//! behaves like `node`: `console.log` goes to stdout, an uncaught error is
//! reported on stderr and exits with status 1.

use std::{fs, process::ExitCode, time::Duration};

use monty_ecma::{JsProgress, JsRepl, JsSuspensionKind};
use monty_types::{
    ExcType, ExtFunctionResult, Language, MontyException, NameLookupResult, NamedValues, PrintWriter, ResourceLimits,
};

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
    let mut progress = repl.feed(&code, &NamedValues::new(), PrintWriter::Stdout);
    loop {
        progress = match progress {
            JsProgress::Complete { .. } => return ExitCode::SUCCESS,
            JsProgress::Failed { error, .. } => {
                for frame in error.traceback() {
                    eprint!("{frame}");
                }
                eprintln!("Uncaught {}", error.summary());
                return ExitCode::FAILURE;
            }
            // there is no host here: unknown names stay unknown and futures never settle
            JsProgress::Suspended(mut suspension) => match suspension.kind() {
                JsSuspensionKind::FunctionCall { function_name, .. } => {
                    let name = function_name.to_owned();
                    suspension.resume_call(ExtFunctionResult::NotFound(name), PrintWriter::Stdout)
                }
                JsSuspensionKind::NameLookup { .. } => {
                    suspension.resume_name_lookup(NameLookupResult::Undefined, PrintWriter::Stdout)
                }
                JsSuspensionKind::OsCall { .. } => {
                    let exc = MontyException::new(ExcType::Error, Some("no host services OS calls".to_owned()));
                    suspension.abort(&exc, PrintWriter::Stdout)
                }
                JsSuspensionKind::ResolveFutures { .. } => {
                    let exc = MontyException::new(
                        ExcType::Error,
                        Some("promises are waiting on host functions, but there is no host".to_owned()),
                    );
                    suspension.abort(&exc, PrintWriter::Stdout)
                }
            },
        };
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
