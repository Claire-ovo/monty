//! A compiled code body and its side tables.

use serde::{Deserialize, Serialize};

use crate::value::Constant;

/// Bytecode plus the tables the VM consults around it.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Code {
    /// Variable-width instructions, addressed by body-relative offsets.
    #[serde(with = "serde_bytes")]
    #[serde(rename = "B")]
    pub bytecode: Vec<u8>,
    /// Immediate constants indexed by `LoadConst`.
    #[serde(rename = "C")]
    pub constants: Vec<Constant>,
    /// Source positions of instructions, for stack traces: sorted by offset,
    /// each entry applying until the next.
    #[serde(rename = "L")]
    pub locations: Vec<LocationEntry>,
    /// Protected ranges and their handlers, innermost first.
    #[serde(rename = "E")]
    pub handlers: Vec<ExceptionEntry>,
    /// The deepest operand stack the body reaches, so a frame can reserve it.
    #[serde(rename = "M")]
    pub max_stack: u16,
}

impl Code {
    /// The source range of the instruction at `offset`, if any was recorded.
    pub fn location(&self, offset: usize) -> Option<(u32, u32)> {
        let offset = u32::try_from(offset).ok()?;
        let index = self.locations.partition_point(|entry| entry.offset <= offset);
        index
            .checked_sub(1)
            .map(|i| (self.locations[i].start, self.locations[i].end))
    }

    /// The innermost handler covering `offset`.
    pub fn handler_for(&self, offset: usize) -> Option<&ExceptionEntry> {
        let offset = u32::try_from(offset).ok()?;
        self.handlers
            .iter()
            .find(|entry| entry.start <= offset && offset < entry.end)
    }
}

/// Maps a bytecode offset to the source range of the expression it executes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct LocationEntry {
    #[serde(rename = "O")]
    pub offset: u32,
    #[serde(rename = "S")]
    pub start: u32,
    #[serde(rename = "E")]
    pub end: u32,
}

/// A protected bytecode range `[start, end)` and the handler that catches
/// anything thrown inside it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct ExceptionEntry {
    #[serde(rename = "S")]
    pub start: u32,
    #[serde(rename = "E")]
    pub end: u32,
    #[serde(rename = "H")]
    pub handler: u32,
    /// Operand stack depth (above the locals) to unwind to before entering the handler.
    #[serde(rename = "D")]
    pub stack_depth: u16,
}
