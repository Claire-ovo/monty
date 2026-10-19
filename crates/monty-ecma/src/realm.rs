//! The realm: the intrinsic objects a session's code runs against.

use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use crate::{heap::HeapId, intern::AtomId, value::JsValue};

/// Heap ids of the intrinsics, created once per session by `builtins::create_realm`.
///
/// The realm owns one reference to each and never releases it: the objects
/// live as long as the heap does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Realm {
    pub global: HeapId,
    /// `Array.prototype.values`, which `arguments` objects iterate with.
    pub array_values: HeapId,
    pub object_prototype: HeapId,
    pub function_prototype: HeapId,
    pub array_prototype: HeapId,
    pub string_prototype: HeapId,
    pub number_prototype: HeapId,
    pub boolean_prototype: HeapId,
    pub symbol_prototype: HeapId,
    pub iterator_prototype: HeapId,
    pub array_iterator_prototype: HeapId,
    pub error_prototype: HeapId,
    pub type_error_prototype: HeapId,
    pub range_error_prototype: HeapId,
    pub reference_error_prototype: HeapId,
    pub syntax_error_prototype: HeapId,
    pub eval_error_prototype: HeapId,
    pub uri_error_prototype: HeapId,
    pub aggregate_error_prototype: HeapId,
    pub map_prototype: HeapId,
    pub set_prototype: HeapId,
    pub weak_map_prototype: HeapId,
    pub weak_set_prototype: HeapId,
    pub map_iterator_prototype: HeapId,
    pub set_iterator_prototype: HeapId,
    pub regexp_prototype: HeapId,
    pub regexp_string_iterator_prototype: HeapId,
    pub generator_prototype: HeapId,
    /// `%GeneratorFunction.prototype%`: the prototype of every `function*` object.
    pub generator_function_prototype: HeapId,
}

impl Realm {
    /// A realm whose every id is entry zero, for a session under construction:
    /// `builtins::create_realm` fills the fields before any of them is read.
    pub fn placeholder() -> Self {
        let zero = HeapId::from_index(0);
        Self {
            global: zero,
            array_values: zero,
            object_prototype: zero,
            function_prototype: zero,
            array_prototype: zero,
            string_prototype: zero,
            number_prototype: zero,
            boolean_prototype: zero,
            symbol_prototype: zero,
            iterator_prototype: zero,
            array_iterator_prototype: zero,
            error_prototype: zero,
            type_error_prototype: zero,
            range_error_prototype: zero,
            reference_error_prototype: zero,
            syntax_error_prototype: zero,
            eval_error_prototype: zero,
            uri_error_prototype: zero,
            aggregate_error_prototype: zero,
            map_prototype: zero,
            set_prototype: zero,
            weak_map_prototype: zero,
            weak_set_prototype: zero,
            map_iterator_prototype: zero,
            set_iterator_prototype: zero,
            regexp_prototype: zero,
            regexp_string_iterator_prototype: zero,
            generator_prototype: zero,
            generator_function_prototype: zero,
        }
    }

    /// The prototype of a builtin error kind.
    pub fn error_prototype(&self, kind: ErrorKind) -> HeapId {
        match kind {
            ErrorKind::Error => self.error_prototype,
            ErrorKind::TypeError => self.type_error_prototype,
            ErrorKind::RangeError => self.range_error_prototype,
            ErrorKind::ReferenceError => self.reference_error_prototype,
            ErrorKind::SyntaxError => self.syntax_error_prototype,
            ErrorKind::EvalError => self.eval_error_prototype,
            ErrorKind::UriError => self.uri_error_prototype,
            ErrorKind::AggregateError => self.aggregate_error_prototype,
        }
    }

    /// The builtin error kind whose prototype `proto` is, if any.
    pub fn error_kind_of(&self, proto: HeapId) -> Option<ErrorKind> {
        ErrorKind::ALL
            .iter()
            .copied()
            .find(|kind| self.error_prototype(*kind) == proto)
    }
}

/// The builtin `Error` constructors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr)]
pub(crate) enum ErrorKind {
    Error,
    TypeError,
    RangeError,
    ReferenceError,
    SyntaxError,
    EvalError,
    #[strum(serialize = "URIError")]
    UriError,
    AggregateError,
}

impl ErrorKind {
    /// Every kind, in constructor order.
    pub const ALL: [Self; 8] = [
        Self::Error,
        Self::TypeError,
        Self::RangeError,
        Self::ReferenceError,
        Self::SyntaxError,
        Self::EvalError,
        Self::UriError,
        Self::AggregateError,
    ];

    /// The constructor's name, which is also the default `name` property.
    pub fn name(self) -> &'static str {
        self.into()
    }

    /// Decodes a `ThrowError` operand.
    pub fn from_u8(operand: u8) -> Option<Self> {
        Self::ALL.get(usize::from(operand)).copied()
    }
}

/// The session's top-level lexical bindings: `let`, `const` and `class`
/// declared at the top of a feed persist across feeds here, by name.
///
/// A later feed redeclaring a name replaces the binding (devtools behaviour)
/// rather than failing, since each feed is compiled as its own module.
/// `var` and function declarations are `globalThis` properties instead.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ScriptScope {
    /// Binding values by index; `Hole` until the declaration runs.
    pub values: Vec<JsValue>,
    /// The name of each binding and whether it is a `const`, parallel to `values`.
    names: Vec<(AtomId, bool)>,
    #[serde(skip)]
    index: AHashMap<AtomId, u32>,
}

impl ScriptScope {
    /// The index of `name`, creating a `Hole` binding when it is new; a
    /// redeclaration takes the new constness.
    pub fn declare(&mut self, name: AtomId, is_const: bool) -> u32 {
        if let Some(index) = self.index.get(&name) {
            self.names[*index as usize].1 = is_const;
            *index
        } else {
            let index = u32::try_from(self.values.len()).expect("more than 2**32 script bindings");
            self.values.push(JsValue::Hole);
            self.names.push((name, is_const));
            self.index.insert(name, index);
            index
        }
    }

    /// An existing binding's index and constness, for a later feed's reference to it.
    pub fn lookup(&self, name: AtomId) -> Option<(u32, bool)> {
        self.index
            .get(&name)
            .map(|index| (*index, self.names[*index as usize].1))
    }
}
