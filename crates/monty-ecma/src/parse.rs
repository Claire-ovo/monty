//! Parsing a feed with oxc, then binding its scopes with `oxc_semantic`.
//!
//! Every feed is parsed as an ES module (`SourceType::mjs()`): module code is
//! strict by definition, which is how the sandbox rejects sloppy-mode-only
//! syntax (`with`, octal literals, duplicate parameters, `delete x`), and it
//! allows top-level `await`. The semantic pass annotates the AST in place —
//! `scope_id`, `symbol_id` and `reference_id` cells — and reports the early
//! errors the parser leaves to it (redeclared `let`, assignment to a constant
//! is a runtime error and stays with the compiler).

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::Parser;
use oxc_semantic::{Scoping, SemanticBuilder};
use oxc_span::SourceType;

use crate::error::CompileError;

/// Parses `source` into `allocator`, returning the annotated program and its scoping.
///
/// The first syntax error wins; oxc's message is used verbatim, so wording
/// follows oxc rather than V8 (see `docs/limitations/javascript.md`).
// TODO(ts-syntax): take a `Language` and pick `SourceType::ts()` for TypeScript
// feeds; the compiler already walks oxc's TypeScript-capable AST.
pub(crate) fn parse<'a>(allocator: &'a Allocator, source: &'a str) -> Result<(Program<'a>, Scoping), CompileError> {
    let parsed = Parser::new(allocator, source, SourceType::mjs()).parse();
    if let Some(error) = parsed.diagnostics.errors().next() {
        return Err(diagnostic_error(error));
    }
    let program = parsed.program;
    let scoping = {
        let semantic = SemanticBuilder::new().with_check_syntax_error(true).build(&program);
        if let Some(error) = semantic.diagnostics.errors().next() {
            return Err(diagnostic_error(error));
        }
        semantic.semantic.into_scoping()
    };
    Ok((program, scoping))
}

/// Turns an oxc diagnostic into a located syntax error.
fn diagnostic_error(error: &oxc_diagnostics::OxcDiagnostic) -> CompileError {
    let span = error
        .labels
        .iter()
        .find(|label| label.primary())
        .or_else(|| error.labels.first())
        .map_or((0, 0), |label| (label.offset(), label.offset() + label.len()));
    CompileError::syntax(error.message.to_string(), span)
}
