//! Session-wide tables: interned strings (atoms), symbols and compiled functions.
//!
//! Atoms are the names JavaScript code spells — identifiers, property keys,
//! string literals — deduplicated so property lookup compares ids, not text.
//! The tables are append-only; ids never move, so bytecode can hold them.

use std::sync::Arc;

use ahash::AHashMap;
use monty_types::ResourceTracker;

use crate::{function::Function, string::JsString};

/// Index of an interned string in [`Interns::atoms`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub(crate) struct AtomId(u32);

impl AtomId {
    /// The raw table index.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The id as a bytecode operand.
    #[inline]
    pub fn as_u16(self) -> Option<u16> {
        u16::try_from(self.0).ok()
    }

    /// Rebuilds an id from a bytecode operand.
    #[inline]
    pub fn from_u16(operand: u16) -> Self {
        Self(u32::from(operand))
    }
}

/// Index of a symbol in [`Interns::symbols`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct SymbolId(u32);

impl SymbolId {
    /// The raw table index.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Index of a compiled function in [`Interns::functions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct FunctionId(u32);

impl FunctionId {
    /// The raw table index.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The id as a bytecode operand.
    #[inline]
    pub fn as_u16(self) -> Option<u16> {
        u16::try_from(self.0).ok()
    }

    /// Rebuilds an id from a bytecode operand.
    #[inline]
    pub fn from_u16(operand: u16) -> Self {
        Self(u32::from(operand))
    }
}

/// A symbol: a unique key with an optional description.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct SymbolInfo {
    /// `Symbol("description")`'s argument, or the well-known name.
    pub description: Option<JsString>,
}

/// Strings the engine itself names, interned at fixed ids when the table is
/// created so builtins and the compiler can refer to them without a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::IntoStaticStr)]
#[repr(u32)]
#[strum(serialize_all = "camelCase")]
pub(crate) enum StaticAtom {
    #[strum(serialize = "")]
    Empty,
    Length,
    Prototype,
    Constructor,
    Name,
    Message,
    Stack,
    Cause,
    ToString,
    ValueOf,
    Undefined,
    Null,
    True,
    False,
    #[strum(serialize = "NaN")]
    NaN,
    #[strum(serialize = "Infinity")]
    Infinity,
    #[strum(serialize = "Object")]
    Object,
    #[strum(serialize = "Array")]
    Array,
    #[strum(serialize = "Function")]
    Function,
    #[strum(serialize = "String")]
    String,
    #[strum(serialize = "Number")]
    Number,
    #[strum(serialize = "Boolean")]
    Boolean,
    #[strum(serialize = "Symbol")]
    Symbol,
    #[strum(serialize = "Error")]
    Error,
    #[strum(serialize = "TypeError")]
    TypeError,
    #[strum(serialize = "RangeError")]
    RangeError,
    #[strum(serialize = "ReferenceError")]
    ReferenceError,
    #[strum(serialize = "SyntaxError")]
    SyntaxError,
    #[strum(serialize = "EvalError")]
    EvalError,
    #[strum(serialize = "URIError")]
    UriError,
    #[strum(serialize = "AggregateError")]
    AggregateError,
    Console,
    Log,
    Warn,
    Info,
    Debug,
    #[strum(serialize = "error")]
    ErrorMethod,
    GlobalThis,
    Arguments,
    Callee,
    Default,
    Next,
    Done,
    Value,
    Return,
    Throw,
    Get,
    Set,
    Writable,
    Enumerable,
    Configurable,
    Push,
    Join,
    Keys,
    Values,
    Entries,
    Then,
    Catch,
    Finally,
    Anonymous,
    Errors,
}

/// The well-known symbols, at fixed ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::IntoStaticStr)]
#[repr(u32)]
#[strum(serialize_all = "camelCase")]
pub(crate) enum WellKnownSymbol {
    Iterator,
    AsyncIterator,
    HasInstance,
    ToPrimitive,
    ToStringTag,
    Species,
    Unscopables,
    IsConcatSpreadable,
    Match,
    MatchAll,
    Replace,
    Search,
    Split,
}

impl WellKnownSymbol {
    /// The symbol's id in every session's table.
    #[inline]
    pub fn id(self) -> SymbolId {
        SymbolId(self as u32)
    }
}

impl StaticAtom {
    /// The atom's id in every session's table.
    #[inline]
    pub fn id(self) -> AtomId {
        AtomId(self as u32)
    }

    /// The text the atom spells.
    pub fn text(self) -> &'static str {
        self.into()
    }
}

/// The session's interned strings, symbols, compiled functions and fed sources.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Interns {
    atoms: Vec<JsString>,
    #[serde(skip)]
    atom_ids: AHashMap<JsString, AtomId>,
    symbols: Vec<SymbolInfo>,
    /// Shared with the frames running them, so a frame borrows no table that interning may grow.
    functions: Vec<Arc<Function>>,
    /// Every fed snippet, by feed index, for error previews and (later) a
    /// type checker's view of the session as one program. TODO(ts-typecheck)
    sources: Vec<Arc<str>>,
}

impl Interns {
    /// A table holding the static atoms and well-known symbols.
    pub fn new() -> Self {
        let mut this = Self {
            atoms: Vec::new(),
            atom_ids: AHashMap::new(),
            symbols: Vec::new(),
            functions: Vec::new(),
            sources: Vec::new(),
        };
        for atom in <StaticAtom as strum::IntoEnumIterator>::iter() {
            let id = this.intern_untracked(JsString::from(atom.text()));
            debug_assert_eq!(id, atom.id(), "static atoms must be interned in declaration order");
        }
        for symbol in <WellKnownSymbol as strum::IntoEnumIterator>::iter() {
            let name: &'static str = symbol.into();
            let id = this.new_symbol(Some(JsString::from(format!("Symbol.{name}").as_str())));
            debug_assert_eq!(
                id,
                symbol.id(),
                "well-known symbols must be created in declaration order"
            );
        }
        this
    }

    /// Interns `text`, charging a new entry to the tracker.
    pub fn intern(&mut self, text: JsString, tracker: &ResourceTracker) -> Result<AtomId, monty_types::ResourceError> {
        if let Some(id) = self.atom_ids.get(&text) {
            return Ok(*id);
        }
        // the string is stored twice (table and lookup map) plus the map entry
        tracker.check_allocation(2 * text.len() * 2 + 64)?;
        Ok(self.intern_untracked(text))
    }

    /// Interns `text` without charging it: the static table and names the
    /// compiler already charged through the source.
    pub fn intern_untracked(&mut self, text: JsString) -> AtomId {
        if let Some(id) = self.atom_ids.get(&text) {
            return *id;
        }
        let id = AtomId(u32::try_from(self.atoms.len()).expect("more than 2**32 atoms"));
        self.atom_ids.insert(text.clone(), id);
        self.atoms.push(text);
        id
    }

    /// Interns Rust text.
    pub fn intern_str(&mut self, text: &str) -> AtomId {
        self.intern_untracked(JsString::from(text))
    }

    /// The text of an atom.
    #[inline]
    pub fn atom(&self, id: AtomId) -> &JsString {
        &self.atoms[id.index()]
    }

    /// Creates a fresh symbol.
    pub fn new_symbol(&mut self, description: Option<JsString>) -> SymbolId {
        let id = SymbolId(u32::try_from(self.symbols.len()).expect("more than 2**32 symbols"));
        self.symbols.push(SymbolInfo { description });
        id
    }

    /// A symbol's description.
    pub fn symbol(&self, id: SymbolId) -> &SymbolInfo {
        &self.symbols[id.index()]
    }

    /// Publishes a compiled function.
    pub fn add_function(&mut self, mut function: Function) -> FunctionId {
        let id = FunctionId(u32::try_from(self.functions.len()).expect("more than 2**32 functions"));
        function.id = id;
        self.functions.push(Arc::new(function));
        id
    }

    /// A compiled function.
    #[inline]
    pub fn function(&self, id: FunctionId) -> &Arc<Function> {
        &self.functions[id.index()]
    }

    /// How many functions are published; the next id a compilation assigns.
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Records a fed snippet; its index is the feed number used in frame names.
    pub fn add_source(&mut self, source: &str) -> usize {
        self.sources.push(Arc::from(source));
        self.sources.len() - 1
    }

    /// A fed snippet's text.
    pub fn source(&self, index: usize) -> Option<&Arc<str>> {
        self.sources.get(index)
    }
}

impl Default for Interns {
    fn default() -> Self {
        Self::new()
    }
}
