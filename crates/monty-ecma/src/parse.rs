//! Parsing a feed with oxc, binding its scopes with `oxc_semantic`, and
//! erasing TypeScript and JSX with `oxc_transformer`.
//!
//! Every feed is parsed as an ES module: module code is strict by definition,
//! which is how the sandbox rejects sloppy-mode-only syntax (`with`, octal
//! literals, duplicate parameters, `delete x`), and it allows top-level
//! `await`. The semantic pass annotates the AST in place — `scope_id`,
//! `symbol_id` and `reference_id` cells — and reports the early errors the
//! parser leaves to it (redeclared `let`; assignment to a constant is a
//! runtime error and stays with the compiler).
//!
//! A TypeScript or JSX feed then goes through oxc's transformer: type
//! annotations, interfaces, `as`, `!` and `declare` disappear, and every JSX
//! element becomes a call of [`JSX_FACTORY`] (fragments name
//! [`JSX_FRAGMENT`]), which the compiler binds to the realm's `jsx`
//! intrinsics rather than to globals. The compiler itself only ever sees
//! plain JavaScript.

use std::{borrow::Cow, path::Path};

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::Parser;
use oxc_semantic::{Scoping, SemanticBuilder};
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer};

use crate::{
    dialect::Dialect,
    error::CompileError,
    source_nesting::{TOO_DEEP, nesting_bound_exceeded},
    typescript::check_strip_only,
};

/// The callee every JSX element lowers to; the compiler maps the name to the
/// realm's `jsx` function, so a script-level `jsx` binding cannot capture it.
pub(crate) const JSX_FACTORY: &str = "__monty_jsx";

/// The element type a JSX fragment lowers to; mapped to the realm's `Fragment`.
pub(crate) const JSX_FRAGMENT: &str = "__monty_fragment";

/// Parses `source` into `allocator` as `dialect`, returning the annotated
/// program and its scoping; `script_name` labels the transformer's diagnostics.
///
/// The first syntax error wins; oxc's message is used verbatim, so wording
/// follows oxc rather than V8 (see `docs/limitations/javascript.md`), except
/// for the TypeScript constructs `check_strip_only` rejects with Node's words.
pub(crate) fn parse<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    dialect: Dialect,
    script_name: &str,
) -> Result<(Program<'a>, Scoping), CompileError> {
    if let Some(offset) = nesting_bound_exceeded(source) {
        let offset = u32::try_from(offset).unwrap_or(u32::MAX);
        return Err(CompileError::syntax(TOO_DEEP, (offset, offset.saturating_add(1))));
    }
    let parsed = Parser::new(allocator, source, dialect.source_type()).parse();
    if let Some(error) = parsed.diagnostics.errors().next() {
        return Err(diagnostic_error(error));
    }
    let mut program = parsed.program;
    let scoping = {
        let semantic = SemanticBuilder::new().with_check_syntax_error(true).build(&program);
        if let Some(error) = semantic.diagnostics.errors().next() {
            return Err(diagnostic_error(error));
        }
        semantic.semantic.into_scoping()
    };
    if !dialect.needs_transform() {
        return Ok((program, scoping));
    }
    if dialect.typescript {
        check_strip_only(&program)?;
    }
    // `/** @jsx */` pragma comments would redirect the lowering to an arbitrary
    // name; the sandbox's factory is not configurable
    program.comments.clear();
    let options = transform_options();
    let transformed =
        Transformer::new(allocator, Path::new(script_name), &options).build_with_scoping(scoping, &mut program);
    if let Some(error) = transformed.diagnostics.errors().next() {
        return Err(diagnostic_error(error));
    }
    Ok((program, transformed.scoping))
}

/// Type erasure and the classic JSX lowering, with every other transform off
/// (the engine runs the latest syntax natively).
fn transform_options() -> TransformOptions {
    let mut options = TransformOptions::default();
    options.typescript.jsx_pragma = Cow::Borrowed(JSX_FACTORY);
    options.typescript.jsx_pragma_frag = Cow::Borrowed(JSX_FRAGMENT);
    options.jsx = JsxOptions {
        runtime: JsxRuntime::Classic,
        pragma: Some(JSX_FACTORY.to_owned()),
        pragma_frag: Some(JSX_FRAGMENT.to_owned()),
        // `pure` only annotates for bundlers, and the display-name plugin
        // rewrites `createReactClass` calls
        pure: false,
        display_name_plugin: false,
        throw_if_namespace: false,
        ..JsxOptions::enable()
    };
    options
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
