//! The builtins: every native function as a [`NativeFn`] variant (a stable
//! id, so function objects serialize without addresses) and the realm setup
//! that wires them into the intrinsic objects.
//!
//! Each submodule implements one area with `fn(vm, this, args, construct)`
//! natives and an `install` that defines them on their prototype or
//! constructor; `create_realm` calls the installs in dependency order.
#![expect(clippy::unnecessary_wraps, reason = "every native shares the `NativeImpl` signature")]

mod array;
mod console;
mod error;
mod function;
mod global;
mod json;
mod math;
mod number;
mod object;
mod string;

use serde::{Deserialize, Serialize};

use crate::{
    error::RunResult,
    heap::{FunctionObject, HeapId, JsHeapData, JsObject, ObjectClass, PropFlags, PropertyKey},
    intern::{StaticAtom, WellKnownSymbol},
    value::JsValue,
    vm::{Vm, number::to_usize},
};

/// The signature every native shares: `this`, the arguments (borrowed; the
/// caller releases them) and whether it was invoked with `new`.
pub(crate) type NativeImpl = fn(&mut Vm<'_>, &JsValue, &[JsValue], bool) -> RunResult<JsValue>;

/// Declares the native table: variant, JavaScript name, `length`, whether
/// `new` is allowed, and the Rust function.
macro_rules! natives {
    ($( $variant:ident => ($name:expr, $length:expr, $ctor:expr, $func:path) ),* $(,)?) => {
        /// A builtin function, identified by variant rather than address.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub(crate) enum NativeFn {
            $( $variant, )*
        }

        impl NativeFn {
            /// The function's `name` property.
            pub fn name(self) -> &'static str {
                match self { $( Self::$variant => $name, )* }
            }

            /// The function's `length` property.
            pub fn length(self) -> u16 {
                match self { $( Self::$variant => $length, )* }
            }

            /// Whether `new` may call it.
            pub fn is_constructor(self) -> bool {
                match self { $( Self::$variant => $ctor, )* }
            }

            fn implementation(self) -> NativeImpl {
                match self { $( Self::$variant => $func, )* }
            }
        }
    };
}

natives! {
    // console
    ConsoleLog => ("log", 0, false, console::log),
    ConsoleError => ("error", 0, false, console::error),
    ConsoleWarn => ("warn", 0, false, console::error),
    ConsoleInfo => ("info", 0, false, console::log),
    ConsoleDebug => ("debug", 0, false, console::log),
    // Object
    Object => ("Object", 1, true, object::constructor),
    ObjectKeys => ("keys", 1, false, object::keys),
    ObjectValues => ("values", 1, false, object::values),
    ObjectEntries => ("entries", 1, false, object::entries),
    ObjectAssign => ("assign", 2, false, object::assign),
    ObjectFreeze => ("freeze", 1, false, object::freeze),
    ObjectIsFrozen => ("isFrozen", 1, false, object::is_frozen),
    ObjectCreate => ("create", 2, false, object::create),
    ObjectGetPrototypeOf => ("getPrototypeOf", 1, false, object::get_prototype_of),
    ObjectSetPrototypeOf => ("setPrototypeOf", 2, false, object::set_prototype_of),
    ObjectDefineProperty => ("defineProperty", 3, false, object::define_property),
    ObjectGetOwnPropertyNames => ("getOwnPropertyNames", 1, false, object::get_own_property_names),
    ObjectFromEntries => ("fromEntries", 1, false, object::from_entries),
    ObjectHasOwn => ("hasOwn", 2, false, object::has_own),
    ObjectIs => ("is", 2, false, object::is),
    ObjectProtoHasOwnProperty => ("hasOwnProperty", 1, false, object::proto_has_own_property),
    ObjectProtoToString => ("toString", 0, false, object::proto_to_string),
    ObjectProtoValueOf => ("valueOf", 0, false, object::proto_value_of),
    ObjectProtoIsPrototypeOf => ("isPrototypeOf", 1, false, object::proto_is_prototype_of),
    ObjectProtoPropertyIsEnumerable => ("propertyIsEnumerable", 1, false, object::proto_property_is_enumerable),
    // Function
    Function => ("Function", 1, true, function::constructor),
    FunctionProtoCall => ("call", 1, false, function::proto_call),
    FunctionProtoApply => ("apply", 2, false, function::proto_apply),
    FunctionProtoBind => ("bind", 1, false, function::proto_bind),
    FunctionProtoToString => ("toString", 0, false, function::proto_to_string),
    // Array
    Array => ("Array", 1, true, array::constructor),
    ArrayIsArray => ("isArray", 1, false, array::is_array),
    ArrayFrom => ("from", 1, false, array::from),
    ArrayOf => ("of", 0, false, array::of),
    ArrayProtoPush => ("push", 1, false, array::push),
    ArrayProtoPop => ("pop", 0, false, array::pop),
    ArrayProtoShift => ("shift", 0, false, array::shift),
    ArrayProtoUnshift => ("unshift", 1, false, array::unshift),
    ArrayProtoSlice => ("slice", 2, false, array::slice),
    ArrayProtoSplice => ("splice", 2, false, array::splice),
    ArrayProtoIndexOf => ("indexOf", 1, false, array::index_of),
    ArrayProtoLastIndexOf => ("lastIndexOf", 1, false, array::last_index_of),
    ArrayProtoIncludes => ("includes", 1, false, array::includes),
    ArrayProtoJoin => ("join", 1, false, array::join),
    ArrayProtoMap => ("map", 1, false, array::map),
    ArrayProtoFilter => ("filter", 1, false, array::filter),
    ArrayProtoForEach => ("forEach", 1, false, array::for_each),
    ArrayProtoReduce => ("reduce", 1, false, array::reduce),
    ArrayProtoReduceRight => ("reduceRight", 1, false, array::reduce_right),
    ArrayProtoFind => ("find", 1, false, array::find),
    ArrayProtoFindIndex => ("findIndex", 1, false, array::find_index),
    ArrayProtoFindLast => ("findLast", 1, false, array::find_last),
    ArrayProtoFindLastIndex => ("findLastIndex", 1, false, array::find_last_index),
    ArrayProtoSome => ("some", 1, false, array::some),
    ArrayProtoEvery => ("every", 1, false, array::every),
    ArrayProtoConcat => ("concat", 1, false, array::concat),
    ArrayProtoReverse => ("reverse", 0, false, array::reverse),
    ArrayProtoSort => ("sort", 1, false, array::sort),
    ArrayProtoFlat => ("flat", 0, false, array::flat),
    ArrayProtoFlatMap => ("flatMap", 1, false, array::flat_map),
    ArrayProtoFill => ("fill", 1, false, array::fill),
    ArrayProtoAt => ("at", 1, false, array::at),
    ArrayProtoKeys => ("keys", 0, false, array::keys),
    ArrayProtoValues => ("values", 0, false, array::values),
    ArrayProtoEntries => ("entries", 0, false, array::entries),
    ArrayProtoToString => ("toString", 0, false, array::to_string),
    ArrayIteratorNext => ("next", 0, false, array::iterator_next),
    IteratorProtoIterator => ("[Symbol.iterator]", 0, false, array::iterator_self),
    // String
    String => ("String", 1, true, string::constructor),
    StringFromCharCode => ("fromCharCode", 1, false, string::from_char_code),
    StringProtoCharAt => ("charAt", 1, false, string::char_at),
    StringProtoCharCodeAt => ("charCodeAt", 1, false, string::char_code_at),
    StringProtoCodePointAt => ("codePointAt", 1, false, string::code_point_at),
    StringProtoIndexOf => ("indexOf", 1, false, string::index_of),
    StringProtoLastIndexOf => ("lastIndexOf", 1, false, string::last_index_of),
    StringProtoIncludes => ("includes", 1, false, string::includes),
    StringProtoStartsWith => ("startsWith", 1, false, string::starts_with),
    StringProtoEndsWith => ("endsWith", 1, false, string::ends_with),
    StringProtoSlice => ("slice", 2, false, string::slice),
    StringProtoSubstring => ("substring", 2, false, string::substring),
    StringProtoToUpperCase => ("toUpperCase", 0, false, string::to_upper_case),
    StringProtoToLowerCase => ("toLowerCase", 0, false, string::to_lower_case),
    StringProtoTrim => ("trim", 0, false, string::trim),
    StringProtoTrimStart => ("trimStart", 0, false, string::trim_start),
    StringProtoTrimEnd => ("trimEnd", 0, false, string::trim_end),
    StringProtoSplit => ("split", 2, false, string::split),
    StringProtoRepeat => ("repeat", 1, false, string::repeat),
    StringProtoPadStart => ("padStart", 2, false, string::pad_start),
    StringProtoPadEnd => ("padEnd", 2, false, string::pad_end),
    StringProtoReplace => ("replace", 2, false, string::replace),
    StringProtoReplaceAll => ("replaceAll", 2, false, string::replace_all),
    StringProtoAt => ("at", 1, false, string::at),
    StringProtoConcat => ("concat", 1, false, string::concat),
    StringProtoToString => ("toString", 0, false, string::to_string),
    StringProtoLocaleCompare => ("localeCompare", 1, false, string::locale_compare),
    StringProtoIterator => ("[Symbol.iterator]", 0, false, string::iterator),
    // Number and Boolean
    Number => ("Number", 1, true, number::constructor),
    NumberIsInteger => ("isInteger", 1, false, number::is_integer),
    NumberIsFinite => ("isFinite", 1, false, number::is_finite),
    NumberIsNaN => ("isNaN", 1, false, number::is_nan),
    NumberIsSafeInteger => ("isSafeInteger", 1, false, number::is_safe_integer),
    NumberProtoToString => ("toString", 1, false, number::to_string),
    NumberProtoToFixed => ("toFixed", 1, false, number::to_fixed),
    NumberProtoToPrecision => ("toPrecision", 1, false, number::to_precision),
    NumberProtoValueOf => ("valueOf", 0, false, number::value_of),
    Boolean => ("Boolean", 1, true, number::boolean_constructor),
    BooleanProtoToString => ("toString", 0, false, number::boolean_to_string),
    BooleanProtoValueOf => ("valueOf", 0, false, number::boolean_value_of),
    Symbol => ("Symbol", 0, false, number::symbol_constructor),
    SymbolProtoToString => ("toString", 0, false, number::symbol_to_string),
    SymbolProtoDescription => ("description", 0, false, number::symbol_description),
    // Math
    MathAbs => ("abs", 1, false, math::abs),
    MathFloor => ("floor", 1, false, math::floor),
    MathCeil => ("ceil", 1, false, math::ceil),
    MathRound => ("round", 1, false, math::round),
    MathTrunc => ("trunc", 1, false, math::trunc),
    MathSign => ("sign", 1, false, math::sign),
    MathSqrt => ("sqrt", 1, false, math::sqrt),
    MathCbrt => ("cbrt", 1, false, math::cbrt),
    MathPow => ("pow", 2, false, math::pow),
    MathMin => ("min", 2, false, math::min),
    MathMax => ("max", 2, false, math::max),
    MathRandom => ("random", 0, false, math::random),
    MathLog => ("log", 1, false, math::log),
    MathLog2 => ("log2", 1, false, math::log2),
    MathLog10 => ("log10", 1, false, math::log10),
    MathExp => ("exp", 1, false, math::exp),
    MathSin => ("sin", 1, false, math::sin),
    MathCos => ("cos", 1, false, math::cos),
    MathTan => ("tan", 1, false, math::tan),
    MathAsin => ("asin", 1, false, math::asin),
    MathAcos => ("acos", 1, false, math::acos),
    MathAtan => ("atan", 1, false, math::atan),
    MathAtan2 => ("atan2", 2, false, math::atan2),
    MathHypot => ("hypot", 2, false, math::hypot),
    // JSON
    JsonStringify => ("stringify", 3, false, json::stringify),
    JsonParse => ("parse", 2, false, json::parse),
    // Errors
    Error => ("Error", 1, true, error::error),
    TypeError => ("TypeError", 1, true, error::type_error),
    RangeError => ("RangeError", 1, true, error::range_error),
    ReferenceError => ("ReferenceError", 1, true, error::reference_error),
    SyntaxError => ("SyntaxError", 1, true, error::syntax_error),
    EvalError => ("EvalError", 1, true, error::eval_error),
    UriError => ("URIError", 1, true, error::uri_error),
    AggregateError => ("AggregateError", 2, true, error::aggregate_error),
    ErrorProtoToString => ("toString", 0, false, error::proto_to_string),
    // globals
    ParseInt => ("parseInt", 2, false, global::parse_int),
    ParseFloat => ("parseFloat", 1, false, global::parse_float),
    IsNaN => ("isNaN", 1, false, global::is_nan),
    IsFinite => ("isFinite", 1, false, global::is_finite),
    BigInt => ("BigInt", 1, false, global::big_int),
}

/// Runs a native.
pub(crate) fn dispatch(
    vm: &mut Vm<'_>,
    native: NativeFn,
    this: &JsValue,
    args: &[JsValue],
    construct: bool,
) -> RunResult<JsValue> {
    native.implementation()(vm, this, args, construct)
}

/// The `i`th argument, `undefined` when absent.
pub(crate) fn arg(args: &[JsValue], i: usize) -> &JsValue {
    args.get(i).unwrap_or(&JsValue::Undefined)
}

/// Builds the realm: prototypes, constructors and the global object, into
/// the placeholder realm of `vm.session`.
pub(crate) fn create_realm(vm: &mut Vm<'_>) {
    // the roots of every prototype chain come first, since every allocation reads them
    let object_prototype = raw_object(vm, None);
    vm.session.realm.object_prototype = object_prototype;
    let function_prototype = vm.session.heap.allocate(JsHeapData::Object(JsObject::with_class(
        Some(object_prototype),
        ObjectClass::Function(FunctionObject::Native(NativeFn::FunctionProtoToString)),
    )));
    vm.session.heap.inc_ref(object_prototype);
    vm.session.realm.function_prototype = function_prototype;
    let global = raw_object(vm, Some(object_prototype));
    vm.session.realm.global = global;

    object::install(vm, global, object_prototype);
    function::install(vm, global, function_prototype);
    array::install(vm, global);
    string::install(vm, global);
    number::install(vm, global);
    error::install(vm, global);
    math::install(vm, global);
    json::install(vm, global);
    console::install(vm, global);
    global::install(vm, global);
}

/// Allocates a bare ordinary object (an owned reference to `proto` is taken).
pub(crate) fn raw_object(vm: &mut Vm<'_>, proto: Option<HeapId>) -> HeapId {
    if let Some(proto) = proto {
        vm.session.heap.inc_ref(proto);
    }
    vm.session.heap.allocate(JsHeapData::Object(JsObject::ordinary(proto)))
}

/// A new ordinary object with `Object.prototype`.
pub(crate) fn plain_object(vm: &mut Vm<'_>) -> HeapId {
    let proto = vm.session.realm.object_prototype;
    raw_object(vm, Some(proto))
}

/// Defines `target[native.name()]` as a native function (hidden, as builtins are).
pub(crate) fn install_function(vm: &mut Vm<'_>, target: HeapId, native: NativeFn) -> HeapId {
    let name = vm.session.interns.intern_str(native.name());
    let function = vm.new_function_object(FunctionObject::Native(native), name, native.length());
    vm.define_with_flags(
        target,
        PropertyKey::Atom(name),
        JsValue::Ref(function),
        PropFlags::HIDDEN,
    );
    function
}

/// Defines `target[symbol]` as a native function.
pub(crate) fn install_symbol_function(
    vm: &mut Vm<'_>,
    target: HeapId,
    symbol: WellKnownSymbol,
    native: NativeFn,
) -> HeapId {
    let name = vm.session.interns.intern_str(native.name());
    let function = vm.new_function_object(FunctionObject::Native(native), name, native.length());
    vm.define_with_flags(
        target,
        PropertyKey::Symbol(symbol.id()),
        JsValue::Ref(function),
        PropFlags::HIDDEN,
    );
    function
}

/// Defines a data property by name with the given attributes.
pub(crate) fn install_value(vm: &mut Vm<'_>, target: HeapId, name: &str, value: JsValue, flags: PropFlags) {
    let name = vm.session.interns.intern_str(name);
    vm.define_with_flags(target, PropertyKey::Atom(name), value, flags);
}

/// Defines a getter by name.
pub(crate) fn install_getter(vm: &mut Vm<'_>, target: HeapId, name: &str, native: NativeFn) {
    let atom = vm.session.interns.intern_str(name);
    let function = vm.new_function_object(FunctionObject::Native(native), atom, 0);
    vm.define_accessor(target, PropertyKey::Atom(atom), JsValue::Ref(function), false);
}

/// Creates a constructor on the global object linked to its prototype:
/// `ctor.prototype` (frozen) and `proto.constructor` (hidden).
pub(crate) fn install_constructor(vm: &mut Vm<'_>, global: HeapId, native: NativeFn, prototype: HeapId) -> HeapId {
    let constructor = install_function(vm, global, native);
    vm.session.heap.inc_ref(prototype);
    vm.define_with_flags(
        constructor,
        PropertyKey::Atom(StaticAtom::Prototype.id()),
        JsValue::Ref(prototype),
        PropFlags::NONE,
    );
    vm.session.heap.inc_ref(constructor);
    vm.define_with_flags(
        prototype,
        PropertyKey::Atom(StaticAtom::Constructor.id()),
        JsValue::Ref(constructor),
        PropFlags::HIDDEN,
    );
    constructor
}

/// `ToIntegerOrInfinity` of an argument, `default` when absent.
pub(crate) fn integer_arg(vm: &mut Vm<'_>, args: &[JsValue], i: usize, default: f64) -> RunResult<f64> {
    match args.get(i) {
        None | Some(JsValue::Undefined) => Ok(default),
        Some(value) => vm.coerce_integer(value),
    }
}

/// Resolves a relative index (`slice`-style: negatives count from the end).
pub(crate) fn relative_index(index: f64, len: usize) -> usize {
    let len_f = len as f64;
    let resolved = if index < 0.0 {
        (len_f + index).max(0.0)
    } else {
        index.min(len_f)
    };
    to_usize(resolved)
}

/// Throws when `value` is not callable, naming it as V8 does for a builtin's
/// callback argument: the type, then the value for primitives.
pub(crate) fn require_callable(vm: &mut Vm<'_>, value: &JsValue) -> RunResult<()> {
    if vm.is_callable(value) {
        return Ok(());
    }
    let shown = match value {
        JsValue::Undefined | JsValue::Hole => "undefined".to_owned(),
        JsValue::Null => "object null".to_owned(),
        JsValue::Bool(b) => format!("boolean {b}"),
        JsValue::Number(_) => format!("number {}", vm.inspect(value)?),
        JsValue::Atom(_) | JsValue::Str(_) => format!("string \"{}\"", vm.string_of(value).expect("string")),
        JsValue::Symbol(_) => "symbol".to_owned(),
        JsValue::BigInt(_) => "bigint".to_owned(),
        JsValue::Ref(_) => "object".to_owned(),
    };
    Err(vm.type_error(&format!("{shown} is not a function")))
}

/// The object a method was called on, or a `TypeError` naming the method.
pub(crate) fn this_object(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<HeapId> {
    match this {
        JsValue::Ref(id) => Ok(*id),
        _ => Err(vm.type_error(&format!("{method} called on non-object"))),
    }
}

/// Interns a name for a property key.
pub(crate) fn key(vm: &mut Vm<'_>, name: &str) -> PropertyKey {
    PropertyKey::Atom(vm.session.interns.intern_str(name))
}
