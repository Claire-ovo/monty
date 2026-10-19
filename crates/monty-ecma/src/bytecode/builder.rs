//! Emits bytecode: operand encoding, forward/backward jumps, source
//! locations, exception ranges and operand stack-depth tracking.

use crate::{
    bytecode::{
        code::{Code, ExceptionEntry, LocationEntry},
        op::{Opcode, OperandShape},
    },
    error::CompileError,
    value::Constant,
};

/// Builds one [`Code`] body.
///
/// Stack depth is tracked per instruction so `max_stack` and exception
/// handlers' unwind depths come out right; the compiler tells the builder the
/// effect of the few opcodes whose effect depends on an operand through
/// [`emit_with_effect`](Self::emit_with_effect).
pub(crate) struct CodeBuilder {
    bytecode: Vec<u8>,
    constants: Vec<Constant>,
    locations: Vec<LocationEntry>,
    handlers: Vec<ExceptionEntry>,
    /// Current operand stack depth, `None` after an unconditional jump or
    /// return until a label is bound.
    depth: Option<u16>,
    max_depth: u16,
    /// The source range to record for the next emitted instruction, when it changed.
    pending_location: Option<(u32, u32)>,
    last_location: Option<(u32, u32)>,
}

/// A forward jump awaiting its target.
#[must_use = "a jump must be patched to its target"]
#[derive(Debug)]
pub(crate) struct JumpLabel {
    /// Offset of the i16 operand to patch.
    operand_at: usize,
    /// Stack depth at the jump, which the target must match.
    depth: Option<u16>,
}

/// A bound position earlier in the bytecode, for backward jumps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Target {
    offset: usize,
    depth: u16,
}

impl CodeBuilder {
    pub fn new() -> Self {
        Self {
            bytecode: Vec::new(),
            constants: Vec::new(),
            locations: Vec::new(),
            handlers: Vec::new(),
            depth: Some(0),
            max_depth: 0,
            pending_location: None,
            last_location: None,
        }
    }

    /// Records the source range of the instructions emitted from now on.
    pub fn set_location(&mut self, start: u32, end: u32) {
        if self.last_location != Some((start, end)) {
            self.pending_location = Some((start, end));
        }
    }

    /// The offset the next instruction will have.
    pub fn offset(&self) -> usize {
        self.bytecode.len()
    }

    /// The current operand stack depth; `None` in unreachable code.
    pub fn depth(&self) -> Option<u16> {
        self.depth
    }

    /// Whether the code position is unreachable (after a jump, return or throw).
    pub fn is_dead(&self) -> bool {
        self.depth.is_none()
    }

    /// Emits an opcode with no operand and its standard stack effect.
    pub fn emit(&mut self, op: Opcode) -> Result<(), CompileError> {
        debug_assert_eq!(op.operand_shape(), OperandShape::None, "{op:?} takes an operand");
        self.apply_effect(op.stack_effect(0))?;
        self.write_op(op);
        Ok(())
    }

    /// Emits an opcode with a one-byte operand.
    pub fn emit_u8(&mut self, op: Opcode, operand: u8) -> Result<(), CompileError> {
        debug_assert_eq!(op.operand_shape(), OperandShape::U8, "{op:?} does not take a u8");
        self.apply_effect(op.stack_effect(i32::from(operand)))?;
        self.write_op(op);
        self.bytecode.push(operand);
        Ok(())
    }

    /// Emits an opcode with a signed one-byte operand.
    pub fn emit_i8(&mut self, op: Opcode, operand: i8) -> Result<(), CompileError> {
        debug_assert_eq!(op.operand_shape(), OperandShape::I8, "{op:?} does not take an i8");
        self.apply_effect(op.stack_effect(0))?;
        self.write_op(op);
        self.bytecode.push(operand.to_le_bytes()[0]);
        Ok(())
    }

    /// Emits an opcode with a two-byte operand.
    pub fn emit_u16(&mut self, op: Opcode, operand: u16) -> Result<(), CompileError> {
        debug_assert_eq!(op.operand_shape(), OperandShape::U16, "{op:?} does not take a u16");
        self.apply_effect(op.stack_effect(i32::from(operand)))?;
        self.write_op(op);
        self.bytecode.extend_from_slice(&operand.to_le_bytes());
        Ok(())
    }

    /// Emits an opcode with two two-byte operands.
    pub fn emit_u16_u16(&mut self, op: Opcode, first: u16, second: u16) -> Result<(), CompileError> {
        debug_assert_eq!(op.operand_shape(), OperandShape::U16U16, "{op:?} does not take u16 u16");
        self.apply_effect(op.stack_effect(0))?;
        self.write_op(op);
        self.bytecode.extend_from_slice(&first.to_le_bytes());
        self.bytecode.extend_from_slice(&second.to_le_bytes());
        Ok(())
    }

    /// Emits a forward jump; patch the returned label where it should land.
    ///
    /// For a popping jump the depth after the instruction is one less; the
    /// label records the depth at the target, which must agree when bound.
    pub fn emit_jump(&mut self, op: Opcode) -> Result<JumpLabel, CompileError> {
        debug_assert!(op.is_jump(), "{op:?} is not a jump");
        let depth_before = self.depth;
        self.apply_effect(op.stack_effect(0))?;
        // the target sees: the iterator popped (`IteratorNext` when done), the
        // operand kept (the keep-variants), or the fall-through depth
        let target_depth = match op {
            Opcode::IteratorNext => depth_before.map(|depth| depth.saturating_sub(1)),
            op if op.keeps_on_jump() => depth_before,
            _ => self.depth,
        };
        self.write_op(op);
        let operand_at = self.bytecode.len();
        self.bytecode.extend_from_slice(&0i16.to_le_bytes());
        if op == Opcode::Jump {
            self.depth = None;
        }
        Ok(JumpLabel {
            operand_at,
            depth: target_depth,
        })
    }

    /// Binds a forward jump to the current position.
    #[expect(clippy::needless_pass_by_value, reason = "a label is bound exactly once")]
    pub fn patch_jump(&mut self, label: JumpLabel) -> Result<(), CompileError> {
        let from = label.operand_at + 2;
        let to = self.bytecode.len();
        let delta = jump_delta(from, to)?;
        self.bytecode[label.operand_at..label.operand_at + 2].copy_from_slice(&delta.to_le_bytes());
        self.join_depth(label.depth)
    }

    /// Emits a backward jump to a bound target.
    pub fn emit_jump_to(&mut self, op: Opcode, target: Target) -> Result<(), CompileError> {
        debug_assert!(op.is_jump(), "{op:?} is not a jump");
        self.apply_effect(op.stack_effect(0))?;
        self.write_op(op);
        let from = self.bytecode.len() + 2;
        let delta = jump_delta(from, target.offset)?;
        self.bytecode.extend_from_slice(&delta.to_le_bytes());
        if let Some(depth) = self.depth
            && depth != target.depth
        {
            return Err(CompileError::internal("backward jump with a different stack depth"));
        }
        if op == Opcode::Jump {
            self.depth = None;
        }
        Ok(())
    }

    /// Marks the current position as a jump target at the current depth.
    pub fn target(&self) -> Result<Target, CompileError> {
        Ok(Target {
            offset: self.bytecode.len(),
            depth: self
                .depth
                .ok_or_else(|| CompileError::internal("jump target in unreachable code"))?,
        })
    }

    /// Makes an unreachable position reachable again at `depth` (the start of
    /// an exception handler, or code after a `return` that a label reaches).
    pub fn resume_at(&mut self, depth: u16) {
        self.depth = Some(depth);
        self.max_depth = self.max_depth.max(depth);
    }

    /// Adds a constant, deduplicating identical immediates.
    pub fn add_const(&mut self, constant: Constant) -> Result<u16, CompileError> {
        let index = if let Some(index) = self.constants.iter().position(|c| c == &constant) {
            index
        } else {
            self.constants.push(constant);
            self.constants.len() - 1
        };
        u16::try_from(index).map_err(|_| CompileError::TooManyConstants)
    }

    /// Registers a protected range: anything thrown in `[start, end)` lands at
    /// `handler` with the operand stack cut back to `stack_depth`.
    ///
    /// Ranges are appended, so the compiler registers a `try` statement's
    /// handlers after those of every `try` nested inside it (which it does by
    /// construction, finishing inner statements first) and a `catch` before
    /// the `finally` sharing its range: the first match is the innermost.
    pub fn add_handler(
        &mut self,
        start: usize,
        end: usize,
        handler: usize,
        stack_depth: u16,
    ) -> Result<(), CompileError> {
        self.handlers.push(ExceptionEntry {
            start: offset_u32(start)?,
            end: offset_u32(end)?,
            handler: offset_u32(handler)?,
            stack_depth,
        });
        Ok(())
    }

    /// Finishes the body.
    pub fn build(self) -> Code {
        Code {
            bytecode: self.bytecode,
            constants: self.constants,
            locations: self.locations,
            handlers: self.handlers,
            max_stack: self.max_depth,
        }
    }

    fn write_op(&mut self, op: Opcode) {
        if let Some((start, end)) = self.pending_location.take() {
            self.locations.push(LocationEntry {
                offset: u32::try_from(self.bytecode.len()).unwrap_or(u32::MAX),
                start,
                end,
            });
            self.last_location = Some((start, end));
        }
        self.bytecode.push(op as u8);
    }

    /// Applies a stack effect; a negative result is a compiler bug, not a user error.
    fn apply_effect(&mut self, effect: i32) -> Result<(), CompileError> {
        let Some(depth) = self.depth else {
            // unreachable code still has to be well-formed, but its depth is moot
            return Ok(());
        };
        let next = i32::from(depth) + effect;
        let next = u16::try_from(next).map_err(|_| CompileError::internal("operand stack underflow"))?;
        self.max_depth = self.max_depth.max(next);
        self.depth = Some(next);
        Ok(())
    }

    /// Merges the depth a jump arrives with into the current position.
    fn join_depth(&mut self, incoming: Option<u16>) -> Result<(), CompileError> {
        match (self.depth, incoming) {
            (None, incoming) => {
                self.depth = incoming;
                if let Some(depth) = incoming {
                    self.max_depth = self.max_depth.max(depth);
                }
            }
            (Some(current), Some(incoming)) if current != incoming => {
                return Err(CompileError::internal(
                    "jump target reached with different stack depths",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

/// The relative operand of a jump from `from` (the end of the jump
/// instruction) to `to`.
fn jump_delta(from: usize, to: usize) -> Result<i16, CompileError> {
    let magnitude = i16::try_from(from.abs_diff(to)).map_err(|_| CompileError::JumpTooFar)?;
    Ok(if to >= from { magnitude } else { -magnitude })
}

/// Converts a bytecode offset to its table representation.
fn offset_u32(offset: usize) -> Result<u32, CompileError> {
    u32::try_from(offset).map_err(|_| CompileError::CodeTooLarge)
}

impl Opcode {
    /// Net operand stack effect, given the operand the effect depends on
    /// (argument count, array length) where it does.
    pub fn stack_effect(self, operand: i32) -> i32 {
        match self {
            Self::Pop => -1,
            Self::Dup => 1,
            Self::Dup2 => 2,
            Self::Swap | Self::Rot3 | Self::Rot4 | Self::Nop => 0,
            Self::LoadConst
            | Self::LoadUndefined
            | Self::LoadNull
            | Self::LoadTrue
            | Self::LoadFalse
            | Self::LoadInt
            | Self::LoadHole
            | Self::LoadLocal
            | Self::LoadLocalChecked
            | Self::LoadCell
            | Self::LoadCellChecked
            | Self::LoadGlobal
            | Self::LoadGlobalOrUndefined
            | Self::LoadScript
            | Self::NewObject
            | Self::MakeClosure
            | Self::LoadThis
            | Self::PushException => 1,
            Self::StoreLocal
            | Self::StoreCell
            | Self::StoreGlobal
            | Self::DefineGlobal
            | Self::StoreScript
            | Self::ArrayPush
            | Self::ArraySpread
            | Self::DefineOwn
            | Self::CopyDataProperties
            | Self::SetPrototype
            | Self::JumpIfFalse
            | Self::JumpIfTrue
            | Self::Throw
            | Self::Return
            | Self::IteratorClose => -1,
            Self::BuildCell | Self::CopyCell | Self::ThrowConstAssign | Self::CreateArguments => 0,
            Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Div
            | Self::Mod
            | Self::Exp
            | Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::Shl
            | Self::Shr
            | Self::UShr
            | Self::Eq
            | Self::Ne
            | Self::StrictEq
            | Self::StrictNe
            | Self::Lt
            | Self::Le
            | Self::Gt
            | Self::Ge
            | Self::InstanceOf
            | Self::In
            | Self::GetElem
            | Self::DeleteElem
            | Self::SetProp => -1,
            Self::Neg
            | Self::Plus
            | Self::Not
            | Self::BitNot
            | Self::TypeOf
            | Self::IncDec
            | Self::GetProp
            | Self::DeleteProp
            | Self::ToString
            | Self::ToPropertyKey
            | Self::GetIterator
            | Self::ForInStart
            | Self::RequireObjectCoercible
            | Self::MakeMethod
            | Self::Jump
            | Self::ArrayPushHole => 0,
            Self::GetPropKeep | Self::GetElemKeep | Self::IteratorNext => 1,
            Self::SetElem | Self::DefineOwnElem | Self::DefineAccessor => -2,
            // the keep-variants pop on fall-through; the jump path is accounted in `emit_jump`
            Self::JumpIfTrueKeep | Self::JumpIfFalseKeep | Self::JumpIfNotNullishKeep => -1,
            Self::JumpIfNullishKeep | Self::JumpIfUndefinedKeep => 0,
            // `count` values become one array / string
            Self::NewArray | Self::TemplateConcat => 1 - operand,
            // callee, this and `argc` arguments become one result
            Self::Call => -1 - operand,
            Self::CallSpread => -2,
            // callee and `argc` arguments become one result
            Self::New => -operand,
            Self::NewSpread => -1,
            // `argc` arguments become `this`
            Self::SuperCall => 1 - operand,
            Self::MakeClass => -1,
            Self::ReturnUndefined | Self::ThrowError | Self::Rethrow => 0,
        }
    }

    /// Whether a conditional jump leaves its operand on the stack when it jumps.
    pub fn keeps_on_jump(self) -> bool {
        matches!(
            self,
            Self::JumpIfTrueKeep
                | Self::JumpIfFalseKeep
                | Self::JumpIfNotNullishKeep
                | Self::JumpIfNullishKeep
                | Self::JumpIfUndefinedKeep
        )
    }
}

impl Default for CodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
