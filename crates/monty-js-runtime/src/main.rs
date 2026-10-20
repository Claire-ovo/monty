#![doc = include_str!("../README.md")]

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use monty_types::Language;

mod engine;
mod run;
mod subprocess;

/// Bounds worker memory and classifies allocation failure as an exit code
/// rather than an abort, so the pool can report `MemoryError`. Declared here
/// because only a binary may; see the `monty-alloc` crate.
#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

/// Monty JS — a secure JavaScript sandbox written in Rust.
///
/// Run a script with `monty-js <file>` (JavaScript, TypeScript, JSX or TSX by
/// extension) or a snippet with `monty-js -e <code>`.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Execute a program passed as a string (like `node -e`).
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// JavaScript file to execute.
    file: Option<String>,

    /// The language of the file or snippet: `javascript` or `typescript`,
    /// inferred from the file's extension (`.js`, `.jsx`, `.ts`, `.tsx`) and
    /// `javascript` for `-e`.
    #[arg(long)]
    language: Option<Language>,

    /// Maximum heap memory in bytes.
    #[arg(long)]
    max_memory: Option<usize>,

    /// Maximum execution time for the run, in seconds.
    #[arg(long)]
    max_feed_duration: Option<f64>,

    /// Maximum call-stack depth (defaults to 1000).
    #[arg(long)]
    max_recursion_depth: Option<usize>,

    #[command(subcommand)]
    subcommand: Option<Command>,
}

/// Subcommands that switch `monty-js` out of its script-running behaviour.
#[derive(Subcommand)]
enum Command {
    /// Run as a protocol child: read framed protobuf requests on stdin and
    /// write framed events on stdout (see the monty-proto crate). Intended to
    /// be driven by a parent process such as monty-pool, not by hand.
    Subprocess,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.subcommand {
        Some(Command::Subprocess) => {
            if cli.eval.is_some() || cli.file.is_some() {
                eprintln!("error: `subprocess` cannot be combined with a file or `-e`");
                ExitCode::FAILURE
            } else {
                subprocess::run()
            }
        }
        None => run::run(&cli),
    }
}
