//! Type conversions and operators: the abstract operations of ECMA-262 §7
//! (`ToPrimitive`, `ToNumber`, `ToString`, equality, relational comparison)
//! and the arithmetic the binary opcodes dispatch to.

use super::{Vm, number};
use crate::{
    bytecode::Opcode,
    error::RunResult,
    heap::{FunctionObject, HeapId, JsHeapData, ObjectClass, PropertyKey},
    intern::{StaticAtom, WellKnownSymbol},
    string::JsString,
    value::JsValue,
};

/// The hint `ToPrimitive` passes to an object's conversion methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hint {
    Default,
    Number,
    String,
}

impl Vm<'_> {
    // === Conversions ===

    /// `ToBoolean`.
    pub fn to_boolean(&self, value: &JsValue) -> bool {
        match value.truthy_immediate() {
            Some(truthy) => truthy,
            None => match value {
                JsValue::Atom(atom) => !self.session.interns.atom(*atom).is_empty(),
                JsValue::Str(id) => match self.session.heap.get(*id) {
                    JsHeapData::String(string) => !string.is_empty(),
                    _ => true,
                },
                _ => true,
            },
        }
    }

    /// `ToNumber`, consuming the value.
    pub fn coerce_number_owned(&mut self, value: JsValue) -> RunResult<f64> {
        let result = self.coerce_number(&value);
        value.drop_with(&mut self.session.heap);
        result
    }

    /// `ToNumber`.
    pub fn coerce_number(&mut self, value: &JsValue) -> RunResult<f64> {
        Ok(match value {
            JsValue::Undefined | JsValue::Hole => f64::NAN,
            JsValue::Null => 0.0,
            JsValue::Bool(b) => f64::from(u8::from(*b)),
            JsValue::Number(n) => *n,
            JsValue::Atom(_) | JsValue::Str(_) => {
                let string = self.string_of(value).expect("a string value");
                number::string_to_number(&string.to_string_lossy())
            }
            JsValue::Symbol(_) => return Err(self.type_error("Cannot convert a Symbol value to a number")),
            JsValue::BigInt(_) => return Err(self.type_error("Cannot convert a BigInt value to a number")),
            JsValue::Ref(_) => {
                let primitive = self.coerce_primitive(value, Hint::Number)?;
                self.coerce_number_owned(primitive)?
            }
        })
    }

    /// `ToNumeric` then `ToIntegerOrInfinity`.
    pub fn coerce_integer(&mut self, value: &JsValue) -> RunResult<f64> {
        let number = self.coerce_number(value)?;
        Ok(if number.is_nan() { 0.0 } else { number.trunc() })
    }

    /// `ToString` as a Rust-side string.
    pub fn coerce_string(&mut self, value: &JsValue) -> RunResult<JsString> {
        Ok(match value {
            JsValue::Undefined | JsValue::Hole => JsString::from("undefined"),
            JsValue::Null => JsString::from("null"),
            JsValue::Bool(true) => JsString::from("true"),
            JsValue::Bool(false) => JsString::from("false"),
            JsValue::Number(n) => JsString::from(number::number_to_string(*n).as_str()),
            JsValue::Atom(_) | JsValue::Str(_) => self.string_of(value).expect("a string value"),
            JsValue::Symbol(_) => return Err(self.type_error("Cannot convert a Symbol value to a string")),
            JsValue::BigInt(_) => return Err(self.type_error("BigInt is not supported")),
            JsValue::Ref(_) => {
                let primitive = self.coerce_primitive(value, Hint::String)?;
                let result = self.coerce_string(&primitive);
                primitive.drop_with(&mut self.session.heap);
                result?
            }
        })
    }

    /// `ToString` as a string value, consuming the input (a string passes through).
    pub fn coerce_string_value(&mut self, value: JsValue) -> RunResult<JsValue> {
        if value.is_string() {
            return Ok(value);
        }
        let string = self.coerce_string(&value);
        value.drop_with(&mut self.session.heap);
        self.alloc_string(string?)
    }

    /// `ToPrimitive`: a primitive passes through (cloned); an object is asked
    /// via `Symbol.toPrimitive`, else `valueOf` / `toString` in hint order.
    pub fn coerce_primitive(&mut self, value: &JsValue, hint: Hint) -> RunResult<JsValue> {
        let JsValue::Ref(id) = value else {
            return Ok(value.clone_with_heap(&self.session.heap));
        };
        let id = *id;
        let exotic = self.get(value, PropertyKey::Symbol(WellKnownSymbol::ToPrimitive.id()))?;
        if !exotic.is_nullish() {
            let hint_name = match hint {
                Hint::Default => "default",
                Hint::Number => "number",
                Hint::String => "string",
            };
            let arg = self.atom_value(hint_name);
            let result = self.call(exotic, value.clone_with_heap(&self.session.heap), vec![arg])?;
            if result.is_object() {
                result.drop_with(&mut self.session.heap);
                return Err(self.type_error("Cannot convert object to primitive value"));
            }
            return Ok(result);
        }
        exotic.drop_with(&mut self.session.heap);
        let order = if hint == Hint::String {
            [StaticAtom::ToString, StaticAtom::ValueOf]
        } else {
            [StaticAtom::ValueOf, StaticAtom::ToString]
        };
        for method_name in order {
            let method = self.get(value, PropertyKey::Atom(method_name.id()))?;
            if self.is_callable(&method) {
                let result = self.call(method, JsValue::Ref(id).clone_with_heap(&self.session.heap), Vec::new())?;
                if !result.is_object() {
                    return Ok(result);
                }
                result.drop_with(&mut self.session.heap);
            } else {
                method.drop_with(&mut self.session.heap);
            }
        }
        Err(self.type_error("Cannot convert object to primitive value"))
    }

    /// `typeof`.
    pub fn type_of(&self, value: &JsValue) -> &'static str {
        if let Some(name) = value.primitive_type_of() {
            name
        } else {
            let JsValue::Ref(id) = value else { unreachable!() };
            if self.is_callable_id(*id) { "function" } else { "object" }
        }
    }

    pub fn is_callable(&self, value: &JsValue) -> bool {
        matches!(value, JsValue::Ref(id) if self.is_callable_id(*id))
    }

    pub fn is_callable_id(&self, id: HeapId) -> bool {
        self.object(id).is_some_and(super::super::heap::JsObject::is_callable)
    }

    // === Equality ===

    /// `IsStrictlyEqual` (`===`).
    #[expect(clippy::float_cmp, reason = "IEEE equality is the specification")]
    pub fn strict_equals(&self, a: &JsValue, b: &JsValue) -> bool {
        match (a, b) {
            (JsValue::Number(x), JsValue::Number(y)) => x == y,
            (JsValue::Atom(_) | JsValue::Str(_), JsValue::Atom(_) | JsValue::Str(_)) => self.strings_equal(a, b),
            _ => a == b,
        }
    }

    /// `SameValueZero`: `===` except `NaN` equals itself.
    pub fn same_value_zero(&self, a: &JsValue, b: &JsValue) -> bool {
        match (a, b) {
            (JsValue::Number(x), JsValue::Number(y)) if x.is_nan() && y.is_nan() => true,
            _ => self.strict_equals(a, b),
        }
    }

    /// `SameValue`: `SameValueZero` but `+0` and `-0` differ (`Object.is`).
    pub fn same_value(&self, a: &JsValue, b: &JsValue) -> bool {
        match (a, b) {
            (JsValue::Number(x), JsValue::Number(y)) if *x == 0.0 && *y == 0.0 => {
                x.is_sign_negative() == y.is_sign_negative()
            }
            _ => self.same_value_zero(a, b),
        }
    }

    /// Code-unit equality of two string values.
    pub fn strings_equal(&self, a: &JsValue, b: &JsValue) -> bool {
        match (a, b) {
            (JsValue::Atom(x), JsValue::Atom(y)) => x == y,
            _ => match (self.string_of(a), self.string_of(b)) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
        }
    }

    /// `IsLooselyEqual` (`==`).
    pub fn loose_equals(&mut self, a: &JsValue, b: &JsValue) -> RunResult<bool> {
        Ok(match (a, b) {
            (
                JsValue::Undefined | JsValue::Null | JsValue::Hole,
                JsValue::Undefined | JsValue::Null | JsValue::Hole,
            ) => true,
            (JsValue::Undefined | JsValue::Null | JsValue::Hole, _)
            | (_, JsValue::Undefined | JsValue::Null | JsValue::Hole) => false,
            (JsValue::Number(_), JsValue::Atom(_) | JsValue::Str(_)) => {
                let n = self.coerce_number(b)?;
                self.strict_equals(a, &JsValue::Number(n))
            }
            (JsValue::Atom(_) | JsValue::Str(_), JsValue::Number(_)) => {
                let n = self.coerce_number(a)?;
                self.strict_equals(&JsValue::Number(n), b)
            }
            (JsValue::Bool(x), _) => {
                let x = JsValue::Number(f64::from(u8::from(*x)));
                self.loose_equals(&x, b)?
            }
            (_, JsValue::Bool(y)) => {
                let y = JsValue::Number(f64::from(u8::from(*y)));
                self.loose_equals(a, &y)?
            }
            (JsValue::Ref(_), JsValue::Ref(_)) => a == b,
            (JsValue::Ref(_), _) => {
                let primitive = self.coerce_primitive(a, Hint::Default)?;
                let result = self.loose_equals(&primitive, b);
                primitive.drop_with(&mut self.session.heap);
                result?
            }
            (_, JsValue::Ref(_)) => {
                let primitive = self.coerce_primitive(b, Hint::Default)?;
                let result = self.loose_equals(a, &primitive);
                primitive.drop_with(&mut self.session.heap);
                result?
            }
            _ => self.strict_equals(a, b),
        })
    }

    // === Operators ===

    /// The arithmetic, string and bitwise binary operators, consuming both operands.
    pub fn binary(&mut self, op: Opcode, lhs: JsValue, rhs: JsValue) -> RunResult<JsValue> {
        if op == Opcode::Add {
            return self.add(lhs, rhs);
        }
        let x = self.coerce_number(&lhs);
        lhs.drop_with(&mut self.session.heap);
        let x = x?;
        let y = self.coerce_number_owned(rhs)?;
        Ok(JsValue::Number(match op {
            Opcode::Sub => x - y,
            Opcode::Mul => x * y,
            Opcode::Div => x / y,
            Opcode::Mod => js_remainder(x, y),
            Opcode::Exp => js_pow(x, y),
            Opcode::BitAnd => f64::from(number::to_int32(x) & number::to_int32(y)),
            Opcode::BitOr => f64::from(number::to_int32(x) | number::to_int32(y)),
            Opcode::BitXor => f64::from(number::to_int32(x) ^ number::to_int32(y)),
            Opcode::Shl => f64::from(number::to_int32(x).wrapping_shl(number::to_uint32(y) & 31)),
            Opcode::Shr => f64::from(number::to_int32(x) >> (number::to_uint32(y) & 31)),
            Opcode::UShr => f64::from(number::to_uint32(x) >> (number::to_uint32(y) & 31)),
            _ => unreachable!("not a binary arithmetic opcode"),
        }))
    }

    /// `+`: string concatenation when either primitive is a string.
    fn add(&mut self, lhs: JsValue, rhs: JsValue) -> RunResult<JsValue> {
        let (lhs, rhs) = match (lhs, rhs) {
            (lhs @ JsValue::Number(_), rhs @ JsValue::Number(_)) => (lhs, rhs),
            (lhs, rhs) => {
                let l = self.coerce_primitive(&lhs, Hint::Default);
                lhs.drop_with(&mut self.session.heap);
                let l = match l {
                    Ok(l) => l,
                    Err(err) => {
                        rhs.drop_with(&mut self.session.heap);
                        return Err(err);
                    }
                };
                let r = self.coerce_primitive(&rhs, Hint::Default);
                rhs.drop_with(&mut self.session.heap);
                let r = match r {
                    Ok(r) => r,
                    Err(err) => {
                        l.drop_with(&mut self.session.heap);
                        return Err(err);
                    }
                };
                (l, r)
            }
        };
        if lhs.is_string() || rhs.is_string() {
            return self.concat_strings(vec![lhs, rhs]);
        }
        let x = self.coerce_number_owned(lhs)?;
        let y = self.coerce_number_owned(rhs)?;
        Ok(JsValue::Number(x + y))
    }

    /// Concatenates `ToString` of every part (template literals, `+`).
    pub fn concat_strings(&mut self, parts: Vec<JsValue>) -> RunResult<JsValue> {
        let mut strings = Vec::with_capacity(parts.len());
        let mut total = 0usize;
        let mut failure = None;
        for part in parts {
            if failure.is_none() {
                match self.coerce_string(&part) {
                    Ok(string) => {
                        total += string.len();
                        strings.push(string);
                    }
                    Err(err) => failure = Some(err),
                }
            }
            part.drop_with(&mut self.session.heap);
        }
        if let Some(err) = failure {
            return Err(err);
        }
        self.tracker().check_allocation(total * 2)?;
        let mut units: Vec<u16> = Vec::with_capacity(total);
        for string in &strings {
            units.extend(string.units());
        }
        self.alloc_string(JsString::from_units(&units))
    }

    /// `<`, `<=`, `>`, `>=`, consuming both operands.
    pub fn compare(&mut self, op: Opcode, lhs: JsValue, rhs: JsValue) -> RunResult<bool> {
        let l = self.coerce_primitive(&lhs, Hint::Number);
        lhs.drop_with(&mut self.session.heap);
        let l = match l {
            Ok(l) => l,
            Err(err) => {
                rhs.drop_with(&mut self.session.heap);
                return Err(err);
            }
        };
        let r = self.coerce_primitive(&rhs, Hint::Number);
        rhs.drop_with(&mut self.session.heap);
        let r = match r {
            Ok(r) => r,
            Err(err) => {
                l.drop_with(&mut self.session.heap);
                return Err(err);
            }
        };
        let result = if l.is_string() && r.is_string() {
            let a = self.string_of(&l).expect("string");
            let b = self.string_of(&r).expect("string");
            let ordering = a.cmp(&b);
            Ok(match op {
                Opcode::Lt => ordering.is_lt(),
                Opcode::Le => ordering.is_le(),
                Opcode::Gt => ordering.is_gt(),
                _ => ordering.is_ge(),
            })
        } else {
            match (self.coerce_number(&l), self.coerce_number(&r)) {
                (Ok(x), Ok(y)) => Ok(match op {
                    Opcode::Lt => x < y,
                    Opcode::Le => x <= y,
                    Opcode::Gt => x > y,
                    _ => x >= y,
                }),
                (Err(err), _) | (_, Err(err)) => Err(err),
            }
        };
        l.drop_with(&mut self.session.heap);
        r.drop_with(&mut self.session.heap);
        result
    }

    /// `instanceof`: `rhs.prototype` on `lhs`'s prototype chain.
    pub fn instance_of(&mut self, lhs: &JsValue, rhs: &JsValue) -> RunResult<bool> {
        let JsValue::Ref(constructor) = rhs else {
            return Err(self.type_error("Right-hand side of 'instanceof' is not an object"));
        };
        if !self.is_callable_id(*constructor) {
            return Err(self.type_error("Right-hand side of 'instanceof' is not callable"));
        }
        let target = match self.object(*constructor).map(|o| &o.class) {
            Some(ObjectClass::Function(FunctionObject::Bound { target, .. })) => JsValue::Ref(*target),
            _ => rhs.clone_with_heap(&self.session.heap),
        };
        let prototype = self.get(&target, PropertyKey::Atom(StaticAtom::Prototype.id()));
        target.drop_with(&mut self.session.heap);
        let prototype = prototype?;
        let JsValue::Ref(prototype_id) = prototype else {
            prototype.drop_with(&mut self.session.heap);
            return Err(self.type_error("Function has non-object prototype in instanceof check"));
        };
        prototype.drop_with(&mut self.session.heap);
        let JsValue::Ref(start) = lhs else {
            return Ok(false);
        };
        let mut current = *start;
        loop {
            match self.get_prototype(current) {
                Some(proto) if proto == prototype_id => return Ok(true),
                Some(proto) => current = proto,
                None => return Ok(false),
            }
        }
    }
}

/// `%` with JavaScript's sign rules (the result takes the dividend's sign).
pub(crate) fn js_remainder(x: f64, y: f64) -> f64 {
    if y == 0.0 || x.is_infinite() || x.is_nan() || y.is_nan() {
        f64::NAN
    } else if y.is_infinite() {
        x
    } else {
        x % y
    }
}

/// `**` with `Number::exponentiate`'s special cases (`NaN` for `1 ** Infinity`).
#[expect(clippy::float_cmp, reason = "the special case is exactly ±1")]
pub(crate) fn js_pow(x: f64, y: f64) -> f64 {
    if y.is_nan() || ((x == 1.0 || x == -1.0) && y.is_infinite()) {
        f64::NAN
    } else {
        x.powf(y)
    }
}
