//! The engine's two error channels: compile-time failures and runtime throws.
//!
//! A [`CompileError`] ends a feed before it runs and surfaces as a
//! `SyntaxError` [`MontyException`]. A [`Throw`] is what the VM propagates:
//! either a JavaScript value (any value is throwable) or a resource limit,
//! which no `catch` can intercept.

use monty_types::{CodeLoc, ExcType, MontyException, ResourceError, StackFrame};

use crate::value::JsValue;

/// Why a snippet could not be compiled.
#[derive(Debug)]
pub(crate) enum CompileError {
    /// The parser or the scope checker rejected the source.
    Syntax { message: String, span: (u32, u32) },
    /// Valid JavaScript the engine does not implement yet.
    Unsupported { feature: &'static str, span: (u32, u32) },
    /// A relative jump does not fit its 16-bit operand.
    JumpTooFar,
    /// More constants than a 16-bit index addresses.
    TooManyConstants,
    /// More bindings than a 16-bit slot addresses.
    TooManyLocals,
    /// The bytecode outgrew its 32-bit offsets.
    CodeTooLarge,
    /// A compiler invariant failed; a bug, reported with its message.
    Internal(&'static str),
}

impl CompileError {
    pub fn syntax(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self::Syntax {
            message: message.into(),
            span,
        }
    }

    pub fn unsupported(feature: &'static str, span: (u32, u32)) -> Self {
        Self::Unsupported { feature, span }
    }

    pub fn internal(message: &'static str) -> Self {
        Self::Internal(message)
    }

    /// The exception a host sees: a `SyntaxError` located in `source`.
    pub fn into_exception(self, script_name: &str, source: &str) -> MontyException {
        let (message, span) = match self {
            Self::Syntax { message, span } => (message, Some(span)),
            Self::Unsupported { feature, span } => (format!("{feature} is not supported"), Some(span)),
            Self::JumpTooFar => ("jump offset out of range".to_owned(), None),
            Self::TooManyConstants => ("too many constants in one function".to_owned(), None),
            Self::TooManyLocals => ("too many variables in one function".to_owned(), None),
            Self::CodeTooLarge => ("function body too large".to_owned(), None),
            Self::Internal(message) => (format!("internal compiler error: {message}"), None),
        };
        let traceback = span
            .map(|(start, end)| {
                let mut frame = stack_frame(script_name, source, start, end, None);
                frame.hide_frame_name = true;
                vec![frame]
            })
            .unwrap_or_default();
        MontyException::with_traceback(ExcType::SyntaxError, Some(message), traceback)
    }
}

/// What unwinds the VM.
#[derive(Debug)]
pub(crate) enum Throw {
    /// A thrown JavaScript value; owns one reference when it points into the heap.
    Value(JsValue),
    /// A resource limit: terminal for the feed, uncatchable by `try`.
    Resource(ResourceError),
}

impl From<ResourceError> for Throw {
    fn from(err: ResourceError) -> Self {
        Self::Resource(err)
    }
}

/// The result of anything that runs JavaScript.
pub(crate) type RunResult<T> = Result<T, Throw>;

/// A traceback frame for the byte range `start..end` of `source`.
pub(crate) fn stack_frame(
    script_name: &str,
    source: &str,
    start: u32,
    end: u32,
    frame_name: Option<String>,
) -> StackFrame {
    let (start_loc, line) = locate(source, start);
    let (end_loc, _) = locate(source, end);
    StackFrame {
        filename: script_name.to_owned(),
        start: start_loc,
        end: end_loc,
        frame_name,
        preview_line: Some(line.into()),
        hide_caret: false,
        hide_frame_name: false,
    }
}

/// The line and column of a byte offset, with the text of its line.
///
/// Columns count characters, as CPython-style tracebacks do, and an offset
/// past the end of the source lands on the last line. `CodeLoc::new` takes
/// zero-based positions and reports them one-based.
fn locate(source: &str, offset: u32) -> (CodeLoc, &str) {
    let offset = (offset as usize).min(source.len());
    let line_start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line_end = source[offset..].find('\n').map_or(source.len(), |i| offset + i);
    let line_index = source[..line_start].matches('\n').count();
    let column = source[line_start..offset].chars().count();
    let loc = CodeLoc::new(
        u32::try_from(line_index).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    );
    (loc, &source[line_start..line_end])
}
