//! The JavaScript opcode set.
//!
//! Bytecode is a `Vec<u8>` of one-byte opcodes followed by their operands,
//! the layout Monty's Python VM uses: no suffix means no operand, `U8`/`I8`
//! one byte, `U16`/`I16` two bytes, and compound shapes are listed per opcode.
//! Jumps are relative `i16` offsets from the end of the instruction.

use strum::FromRepr;

/// Opcode discriminant; operands follow in the byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromRepr)]
#[repr(u8)]
pub(crate) enum Opcode {
    // === Stack ===
    /// Discard the top value.
    Pop = 0,
    /// Duplicate the top value.
    Dup = 1,
    /// Duplicate the top two values (`a b` → `a b a b`).
    Dup2 = 2,
    /// Swap the top two values.
    Swap = 3,
    /// Move the top value under the two below it (`a b c` → `c a b`).
    Rot3 = 4,
    /// Move the top value under the three below it (`a b c d` → `d a b c`).
    Rot4 = 5,

    // === Constants ===
    /// Push a constant. Operand: u16 constant index.
    LoadConst = 10,
    LoadUndefined = 11,
    LoadNull = 12,
    LoadTrue = 13,
    LoadFalse = 14,
    /// Push a small integer. Operand: i8.
    LoadInt = 15,
    /// Push the uninitialised marker, for a `let` without an initializer.
    LoadHole = 16,

    // === Locals and cells ===
    /// Push a local slot. Operand: u16 slot.
    LoadLocal = 20,
    /// Push a local slot, throwing `ReferenceError` on the TDZ hole.
    /// Operands: u16 slot, u16 name atom.
    LoadLocalChecked = 21,
    /// Pop into a local slot. Operand: u16 slot.
    StoreLocal = 22,
    /// Allocate a cell holding the hole into a slot. Operand: u16 slot.
    BuildCell = 23,
    /// Push a cell's contents. Operand: u16 slot.
    LoadCell = 24,
    /// Push a cell's contents, throwing `ReferenceError` on the hole.
    /// Operands: u16 slot, u16 name atom.
    LoadCellChecked = 25,
    /// Pop into a cell. Operand: u16 slot.
    StoreCell = 26,
    /// Replace a slot's cell with a fresh cell holding the same value, so
    /// closures created so far keep the old binding (`for (let …)`).
    /// Operand: u16 slot.
    CopyCell = 27,
    /// Throw `TypeError: Assignment to constant variable.` Operand: u16 name atom.
    ThrowConstAssign = 28,

    // === Globals and script scope ===
    /// Push a `globalThis` property, throwing `ReferenceError` when absent.
    /// Operand: u16 name atom.
    LoadGlobal = 30,
    /// Push a `globalThis` property, or `undefined` when absent (`typeof x`).
    /// Operand: u16 name atom.
    LoadGlobalOrUndefined = 31,
    /// Pop into a `globalThis` property that must exist (strict assignment).
    /// Operand: u16 name atom.
    StoreGlobal = 32,
    /// Pop into a `globalThis` property, creating it (`var` / function
    /// declarations at the top level). Operand: u16 name atom.
    DefineGlobal = 33,
    /// Push a script-scope binding, throwing on the hole. Operands: u16 index, u16 name atom.
    LoadScript = 34,
    /// Pop into a script-scope binding. Operand: u16 index.
    StoreScript = 35,

    // === Operators ===
    Add = 40,
    Sub = 41,
    Mul = 42,
    Div = 43,
    Mod = 44,
    Exp = 45,
    BitAnd = 46,
    BitOr = 47,
    BitXor = 48,
    Shl = 49,
    Shr = 50,
    UShr = 51,
    /// Unary `-`.
    Neg = 52,
    /// Unary `+` (`ToNumber`).
    Plus = 53,
    /// Logical `!`.
    Not = 54,
    /// Bitwise `~`.
    BitNot = 55,
    TypeOf = 56,
    /// `++`/`--` on the top value: `ToNumeric` then add the i8 operand.
    IncDec = 57,
    Eq = 60,
    Ne = 61,
    StrictEq = 62,
    StrictNe = 63,
    Lt = 64,
    Le = 65,
    Gt = 66,
    Ge = 67,
    InstanceOf = 68,
    In = 69,

    // === Jumps (operand: i16 relative offset) ===
    Jump = 70,
    /// Pop and jump when falsy.
    JumpIfFalse = 71,
    /// Pop and jump when truthy.
    JumpIfTrue = 72,
    /// Peek: jump when truthy, else pop (`||`).
    JumpIfTrueKeep = 73,
    /// Peek: jump when falsy, else pop (`&&`).
    JumpIfFalseKeep = 74,
    /// Peek: jump when not nullish, else pop (`??`).
    JumpIfNotNullishKeep = 75,
    /// Peek: jump when nullish (optional chaining short-circuit; the value stays).
    JumpIfNullishKeep = 76,
    /// Peek: jump when `undefined` (default parameters / destructuring defaults).
    JumpIfUndefinedKeep = 77,

    // === Properties ===
    /// `obj` → `value`. Operand: u16 name atom.
    GetProp = 80,
    /// `obj` → `obj value`, for method calls. Operand: u16 name atom.
    GetPropKeep = 81,
    /// `obj value` → `value`. Operand: u16 name atom.
    SetProp = 82,
    /// `obj key` → `value`.
    GetElem = 83,
    /// `obj key` → `obj key value`, for `obj[k]++` and compound assignment.
    GetElemKeep = 84,
    /// `obj key value` → `value`.
    SetElem = 85,
    /// `obj` → `bool`. Operand: u16 name atom.
    DeleteProp = 86,
    /// `obj key` → `bool`.
    DeleteElem = 87,
    /// Define an own data property on a literal: `obj value` → `obj`. Operand: u16 name atom.
    DefineOwn = 88,
    /// `obj key value` → `obj`.
    DefineOwnElem = 89,
    /// Define a getter or setter: `obj key fn` → `obj`. Operand: u8 (0 getter, 1 setter).
    DefineAccessor = 90,
    /// Copy own enumerable properties: `obj source` → `obj` (`{...x}`).
    CopyDataProperties = 91,
    /// Define `__proto__: v` in a literal: `obj proto` → `obj`.
    SetPrototype = 92,

    // === Objects, arrays, functions ===
    /// Push a new empty object with `Object.prototype`.
    NewObject = 100,
    /// Push a new array of the top u16 values (in push order). Operand: u16 count.
    NewArray = 101,
    /// `arr value` → `arr`, appending.
    ArrayPush = 102,
    /// `arr` → `arr`, appending a hole.
    ArrayPushHole = 103,
    /// `arr iterable` → `arr`, appending every element (`[...x]`).
    ArraySpread = 104,
    /// Push a closure of a compiled function. Operand: u16 function id.
    MakeClosure = 105,
    /// Push a closure whose `home_object` is the object under it: `obj` → `obj fn`.
    /// Operand: u16 function id.
    MakeMethod = 106,
    /// Push `this` (the frame's receiver slot). Operand: u16 slot.
    LoadThis = 107,
    /// Build a template string from the top u8 values with `ToString`. Operand: u8 count.
    TemplateConcat = 108,
    /// Build a class: `parent_or_hole ctor_fn` → `class`. Operand: u8 flags (1 = has parent).
    MakeClass = 109,

    // === Calls ===
    /// `callee this arg…` → `result`. Operand: u8 argument count.
    Call = 110,
    /// `callee this args_array` → `result` (a call with spread).
    CallSpread = 111,
    /// `callee arg…` → `result`. Operand: u8 argument count.
    New = 112,
    /// `callee args_array` → `result`.
    NewSpread = 113,
    /// Return the top value.
    Return = 114,
    /// Return `undefined`.
    ReturnUndefined = 115,
    /// `this` of a `super(...)` call in a derived constructor: `args…` → `this`.
    SuperCall = 116,

    // === Exceptions ===
    /// Throw the top value.
    Throw = 120,
    /// Throw a builtin error. Operands: u8 error kind, u16 message atom.
    ThrowError = 121,
    /// Push the caught exception (handler entry).
    PushException = 122,
    /// Rethrow the exception a `finally` block saved.
    Rethrow = 123,

    // === Iteration ===
    /// `iterable` → `iterator`: `GetIterator` through `Symbol.iterator`.
    GetIterator = 130,
    /// `iterator` → `iterator value`, or jump when done (popping the iterator). Operand: i16 offset.
    IteratorNext = 131,
    /// `iterator` → (nothing): calls `return` if present.
    IteratorClose = 132,
    /// `obj` → `keys_iterator` for `for…in`.
    ForInStart = 133,

    // === Misc ===
    /// `value` → `string`.
    ToString = 140,
    /// `value` → `property key` (`ToPropertyKey`, for computed members).
    ToPropertyKey = 141,
    /// No-op (`debugger`).
    Nop = 142,
    /// Create the `arguments` object into a slot. Operand: u16 slot.
    CreateArguments = 143,
    /// `value` → `value`: throw `TypeError` when not an object (destructuring `null`).
    RequireObjectCoercible = 144,
}

/// How many operand bytes follow an opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperandShape {
    None,
    U8,
    I8,
    U16,
    I16,
    U16U16,
    U8U16,
}

impl Opcode {
    /// The operand layout of this opcode.
    pub fn operand_shape(self) -> OperandShape {
        use OperandShape as S;
        match self {
            Self::Pop
            | Self::Dup
            | Self::Dup2
            | Self::Swap
            | Self::Rot3
            | Self::Rot4
            | Self::LoadUndefined
            | Self::LoadNull
            | Self::LoadTrue
            | Self::LoadFalse
            | Self::LoadHole
            | Self::Add
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
            | Self::Neg
            | Self::Plus
            | Self::Not
            | Self::BitNot
            | Self::TypeOf
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
            | Self::GetElemKeep
            | Self::SetElem
            | Self::DeleteElem
            | Self::DefineOwnElem
            | Self::CopyDataProperties
            | Self::SetPrototype
            | Self::NewObject
            | Self::ArrayPush
            | Self::ArrayPushHole
            | Self::ArraySpread
            | Self::CallSpread
            | Self::NewSpread
            | Self::Return
            | Self::ReturnUndefined
            | Self::Throw
            | Self::PushException
            | Self::Rethrow
            | Self::GetIterator
            | Self::IteratorClose
            | Self::ForInStart
            | Self::ToString
            | Self::ToPropertyKey
            | Self::Nop
            | Self::RequireObjectCoercible => S::None,
            Self::LoadInt | Self::IncDec => S::I8,
            Self::DefineAccessor | Self::TemplateConcat | Self::MakeClass | Self::Call | Self::New => S::U8,
            Self::SuperCall => S::U8,
            Self::LoadConst
            | Self::LoadLocal
            | Self::StoreLocal
            | Self::BuildCell
            | Self::LoadCell
            | Self::StoreCell
            | Self::CopyCell
            | Self::ThrowConstAssign
            | Self::LoadGlobal
            | Self::LoadGlobalOrUndefined
            | Self::StoreGlobal
            | Self::DefineGlobal
            | Self::StoreScript
            | Self::GetProp
            | Self::GetPropKeep
            | Self::SetProp
            | Self::DeleteProp
            | Self::DefineOwn
            | Self::NewArray
            | Self::MakeClosure
            | Self::MakeMethod
            | Self::LoadThis
            | Self::CreateArguments => S::U16,
            Self::Jump
            | Self::JumpIfFalse
            | Self::JumpIfTrue
            | Self::JumpIfTrueKeep
            | Self::JumpIfFalseKeep
            | Self::JumpIfNotNullishKeep
            | Self::JumpIfNullishKeep
            | Self::JumpIfUndefinedKeep
            | Self::IteratorNext => S::I16,
            Self::LoadLocalChecked | Self::LoadCellChecked | Self::LoadScript => S::U16U16,
            Self::ThrowError => S::U8U16,
        }
    }

    /// Whether the opcode is a relative jump whose operand needs patching.
    pub fn is_jump(self) -> bool {
        self.operand_shape() == OperandShape::I16
    }
}
