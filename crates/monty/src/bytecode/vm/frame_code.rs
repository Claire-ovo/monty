//! Code ownership for active frames, including reclaimable runtime-compiled bodies.

use std::{ops::Deref, sync::Arc};

use crate::{
    bytecode::Code,
    intern::{FunctionId, Interns},
};

/// Ordinary frames borrow stable code; snippets own code until execution finishes.
/// The borrowed arm caches the instruction slice to avoid another code dereference on fetch.
#[derive(Debug)]
pub(super) enum FrameCode<'code> {
    Borrowed { code: &'code Code, bytecode: &'code [u8] },
    Owned(Arc<Code>),
}

impl<'code> FrameCode<'code> {
    /// Borrows a committed function or the running program's module body.
    pub(super) fn borrowed(code: &'code Code) -> Self {
        Self::Borrowed {
            code,
            bytecode: code.bytecode(),
        }
    }

    /// Restores new owned snippets and legacy function-ID-based frames alike.
    pub(super) fn restore(
        interns: &'code Interns,
        module_code: &'code Code,
        function_id: Option<FunctionId>,
        owned: Option<Arc<Code>>,
    ) -> Self {
        match owned {
            Some(code) => Self::Owned(code),
            None => Self::borrowed(function_id.map_or(module_code, |id| &interns.get_function(id).code)),
        }
    }

    /// Instructions for dispatch, without copying an owned snippet's buffer.
    #[inline]
    pub(super) fn bytecode(&self) -> &[u8] {
        match self {
            Self::Borrowed { bytecode, .. } => bytecode,
            Self::Owned(code) => code.bytecode(),
        }
    }

    /// Keeps transient code alive across suspension or a task switch.
    pub(super) fn owned(&self) -> Option<Arc<Code>> {
        match self {
            Self::Borrowed { .. } => None,
            Self::Owned(code) => Some(Arc::clone(code)),
        }
    }
}

impl Deref for FrameCode<'_> {
    type Target = Code;

    fn deref(&self) -> &Code {
        match self {
            Self::Borrowed { code, .. } => code,
            Self::Owned(code) => code,
        }
    }
}
