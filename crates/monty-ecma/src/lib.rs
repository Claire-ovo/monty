//! A sandboxed JavaScript interpreter on Monty's shared heap and wire protocol.
//!
//! The crate is the engine behind the `monty-js` worker: it parses JavaScript
//! with oxc, resolves scopes, compiles to its own bytecode and runs it on a
//! stack VM over `monty-heap`'s reference-counted arena, exchanging values with
//! hosts as `monty_types::MontyObject` graphs. The public surface mirrors the
//! Python interpreter's REPL: [`JsRepl`] holds one session and [`JsProgress`]
//! is what a feed returns.
//!
//! Every snippet runs as strict-mode module code: sloppy-mode features are
//! rejected, and top-level `await` is allowed. See `docs/limitations/javascript.md`
//! for what the engine does differently from Node.

mod builtins;
mod bytecode;
mod compile;
mod error;
mod function;
mod heap;
mod intern;
mod object_bridge;
mod parse;
mod realm;
mod repl;
mod resolve;
mod string;
mod value;
mod vm;

pub use repl::{JsProgress, JsRepl, JsStartError};
