//! The bytecode a snippet compiles to and the builder that emits it.

mod builder;
mod code;
mod op;

pub(crate) use builder::{CodeBuilder, JumpLabel, Target};
pub(crate) use code::Code;
pub(crate) use op::Opcode;
