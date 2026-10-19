//! The JavaScript value representation.

use std::fmt;

use monty_heap::{ContainsHeap, DropWithContext, HeapId};

use crate::{
    heap::{Heap, JsHeapData},
    intern::{AtomId, SymbolId},
};

/// A JavaScript value: an immediate, or a reference into the heap.
///
/// Strings come in two forms — an interned [`Atom`](Self::Atom) (literals,
/// property names, short results) and a heap-allocated [`Str`](Self::Str) — and
/// `BigInt`s and every object live in the heap. Heap references are counted:
/// clone with [`clone_with_heap`](Self::clone_with_heap) and release with
/// `drop_with`, never with `Clone`/`Drop`.
///
/// [`Hole`](Self::Hole) is the uninitialised-binding marker (the temporal dead
/// zone of `let`/`const`, a missing array element); it is never observable by
/// JavaScript code.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum JsValue {
    #[serde(rename = "U")]
    Undefined,
    #[serde(rename = "N")]
    Null,
    #[serde(rename = "H")]
    Hole,
    #[serde(rename = "B")]
    Bool(bool),
    #[serde(rename = "F")]
    Number(f64),
    #[serde(rename = "A")]
    Atom(AtomId),
    #[serde(rename = "S")]
    Str(HeapId),
    #[serde(rename = "Y")]
    Symbol(SymbolId),
    #[serde(rename = "I")]
    BigInt(HeapId),
    #[serde(rename = "R")]
    Ref(HeapId),
}

impl JsValue {
    /// The heap entry this value owns a reference to, if any.
    #[inline]
    pub fn heap_id(&self) -> Option<HeapId> {
        match self {
            Self::Str(id) | Self::BigInt(id) | Self::Ref(id) => Some(*id),
            _ => None,
        }
    }

    /// Copies a value, counting the reference when it points into the heap.
    #[inline]
    pub fn clone_with_heap(&self, heap: &impl ContainsHeap<Data = JsHeapData>) -> Self {
        if let Some(id) = self.heap_id() {
            heap.heap().inc_ref(id);
        }
        self.copy_unchecked()
    }

    /// Bitwise copy; the caller accounts for any reference.
    #[inline]
    fn copy_unchecked(&self) -> Self {
        match self {
            Self::Undefined => Self::Undefined,
            Self::Null => Self::Null,
            Self::Hole => Self::Hole,
            Self::Bool(b) => Self::Bool(*b),
            Self::Number(n) => Self::Number(*n),
            Self::Atom(a) => Self::Atom(*a),
            Self::Str(id) => Self::Str(*id),
            Self::Symbol(s) => Self::Symbol(*s),
            Self::BigInt(id) => Self::BigInt(*id),
            Self::Ref(id) => Self::Ref(*id),
        }
    }

    /// Whether this is `undefined` or `null`.
    #[inline]
    pub fn is_nullish(&self) -> bool {
        matches!(self, Self::Undefined | Self::Null)
    }

    /// Whether this is a string, interned or heap-allocated.
    #[inline]
    pub fn is_string(&self) -> bool {
        matches!(self, Self::Atom(_) | Self::Str(_))
    }

    /// Whether this is an object (functions included).
    #[inline]
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Ref(_))
    }

    /// The `typeof` result for a primitive; objects need the heap to tell a
    /// function from an ordinary object, so they return `None`.
    pub fn primitive_type_of(&self) -> Option<&'static str> {
        match self {
            Self::Undefined | Self::Hole => Some("undefined"),
            Self::Null => Some("object"),
            Self::Bool(_) => Some("boolean"),
            Self::Number(_) => Some("number"),
            Self::Atom(_) | Self::Str(_) => Some("string"),
            Self::Symbol(_) => Some("symbol"),
            Self::BigInt(_) => Some("bigint"),
            Self::Ref(_) => None,
        }
    }

    /// ECMAScript `ToBoolean` for everything but a heap string, whose
    /// emptiness needs the heap; see `Vm::to_boolean`.
    pub fn truthy_immediate(&self) -> Option<bool> {
        match self {
            Self::Undefined | Self::Null | Self::Hole => Some(false),
            Self::Bool(b) => Some(*b),
            Self::Number(n) => Some(*n != 0.0 && !n.is_nan()),
            Self::Symbol(_) | Self::Ref(_) => Some(true),
            Self::Atom(_) | Self::Str(_) | Self::BigInt(_) => None,
        }
    }
}

impl<C: ContainsHeap<Data = JsHeapData>> DropWithContext<C> for JsValue {
    #[inline]
    fn drop_with(self, ctx: &mut C) {
        if let Some(id) = self.heap_id() {
            ctx.heap_mut().dec_ref(id);
        }
    }
}

impl JsValue {
    /// Releases the reference this value may hold; the inherent twin of the
    /// trait method so callers need no import.
    #[inline]
    pub fn drop_with(self, heap: &mut impl ContainsHeap<Data = JsHeapData>) {
        if let Some(id) = self.heap_id() {
            heap.heap_mut().dec_ref(id);
        }
    }
}

/// A `JsValue` that is `Copy` because it never references the heap: the
/// compiler's constant pool holds these.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Constant {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    /// An interned string.
    Atom(AtomId),
}

impl Constant {
    /// The runtime value of the constant.
    #[inline]
    pub fn to_value(self) -> JsValue {
        match self {
            Self::Undefined => JsValue::Undefined,
            Self::Null => JsValue::Null,
            Self::Bool(b) => JsValue::Bool(b),
            Self::Number(n) => JsValue::Number(n),
            Self::Atom(a) => JsValue::Atom(a),
        }
    }
}

impl fmt::Display for Constant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undefined => f.write_str("undefined"),
            Self::Null => f.write_str("null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::Atom(a) => write!(f, "atom#{}", a.index()),
        }
    }
}

/// Keeps the `Heap` alias in scope for the `ContainsHeap` bounds above.
#[expect(dead_code, reason = "documents which heap the value belongs to")]
type Owner = Heap;
