//! A compiled function: its bytecode plus what a call needs to set its frame up.

use serde::{Deserialize, Serialize};

use crate::{bytecode::Code, intern::AtomId};

/// A compiled function body, published to `Interns` once its snippet is admitted.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Function {
    /// The `name` property's value; the empty atom for an anonymous function.
    #[serde(rename = "N")]
    pub name: AtomId,
    /// The body.
    #[serde(rename = "C")]
    pub code: Code,
    /// The `length` property: parameters before the first default or rest.
    #[serde(rename = "L")]
    pub param_count: u16,
    /// Declared parameters, in order; the first `locals` slots.
    #[serde(rename = "A")]
    pub formal_count: u16,
    /// Whether the last formal parameter is a rest parameter.
    #[serde(rename = "R")]
    pub has_rest: bool,
    /// Size of the frame's local region: parameters, hidden slots and every
    /// binding declared in the body, block scopes included.
    #[serde(rename = "S")]
    pub locals: u16,
    /// How the function is called and what `this` means inside it.
    #[serde(rename = "K")]
    pub kind: FunctionKind,
    /// The slot receiving the call's receiver; `None` for an arrow function,
    /// which reads `this` from its enclosing scope like any variable.
    #[serde(rename = "T")]
    pub this_slot: Option<u16>,
    /// The slot receiving `new.target`, if the body reads it.
    #[serde(rename = "W")]
    pub new_target_slot: Option<u16>,
    /// The slot receiving the `arguments` object, if the body reads it.
    #[serde(rename = "G")]
    pub arguments_slot: Option<u16>,
    /// The slot receiving the function object itself: the name binding of a
    /// named function expression, visible only inside its body.
    #[serde(rename = "Z")]
    pub self_slot: Option<u16>,
    /// Bindings of this function's own scope that an inner function captures:
    /// each gets a fresh cell at call time. Block-scoped captured bindings are
    /// not here — the body builds their cells where the block begins.
    #[serde(rename = "E")]
    pub entry_cells: Vec<EntryCell>,
    /// Slots of the *enclosing* frame whose cells this function closes over,
    /// copied into the closure when it is created.
    #[serde(rename = "U")]
    pub captures: Vec<u16>,
    /// This function's slots that receive the captured cells, parallel to `captures`.
    #[serde(rename = "F")]
    pub free_slots: Vec<u16>,
    /// Where the function is in its feed's source, for stack traces.
    #[serde(rename = "P")]
    pub span: SourceSpan,
}

/// A binding that lives in a cell from the moment the frame is set up: the
/// VM wraps whatever the slot holds after parameter binding (an argument,
/// the receiver, or `undefined`) into a fresh cell.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct EntryCell {
    /// The slot that holds the cell reference.
    pub slot: u16,
}

/// How a function may be invoked and what `this` means in its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FunctionKind {
    /// `function f() {}`: callable and constructible.
    Normal,
    /// `() => {}`: no own `this`, not constructible.
    Arrow,
    /// An object or class method, getter or setter: not constructible.
    Method,
    /// A class constructor: only constructible; `derived` when the class `extends`.
    ClassConstructor { derived: bool },
    /// `async function`.
    Async,
    /// `function*`.
    Generator,
    /// `async function*`.
    AsyncGenerator,
}

impl FunctionKind {
    /// Whether `new f()` is allowed.
    pub fn is_constructor(self) -> bool {
        matches!(self, Self::Normal | Self::ClassConstructor { .. })
    }

    /// Whether this is a class constructor, which `new` alone may call.
    pub fn is_class_constructor(self) -> bool {
        matches!(self, Self::ClassConstructor { .. })
    }

    /// A derived constructor's `this` exists only once `super()` returns.
    pub fn is_derived_constructor(self) -> bool {
        matches!(self, Self::ClassConstructor { derived: true })
    }
}

/// A range of a feed's source text, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceSpan {
    /// Which feed the source belongs to (an index into `Interns::sources`).
    pub feed: u32,
    pub start: u32,
    pub end: u32,
}
