//! Destructuring: binding patterns in declarations and parameters, and
//! assignment-target patterns in assignments and loop heads.
//!
//! Every entry point consumes the value on top of the operand stack. Object
//! patterns read properties off a kept copy; array patterns drive an iterator
//! with `IteratorStep`, which hands back `undefined` once exhausted and
//! swaps the iterator for `undefined` so the closing `IteratorClose` only
//! calls `return` on an iterator that was left mid-way, as the specification
//! requires.

use oxc_ast::ast::{
    AssignmentTarget, AssignmentTargetMaybeDefault, AssignmentTargetProperty, BindingPattern, Expression, PropertyKey,
};

use super::{Compiler, atom_operand, expr::Member, symbol_of};
use crate::{
    bytecode::Opcode,
    error::CompileError,
    intern::{AtomId, StaticAtom},
    vm::number::number_to_string,
};

impl<'c> Compiler<'c> {
    /// Binds the top value to a declaration pattern (`const {a, b: [c]} = v`).
    /// `source` names the initializer in the not-iterable error, as V8 does.
    pub(super) fn bind_pattern(
        &mut self,
        pattern: &'c BindingPattern<'c>,
        source: Option<&'c Expression<'c>>,
    ) -> Result<(), CompileError> {
        self.bind_pattern_at(pattern, source, true)
    }

    /// [`bind_pattern`](Self::bind_pattern) for a nested pattern, where a
    /// nullish value fails on the property read (V8's "Cannot read
    /// properties of null") rather than with the destructuring message.
    fn bind_pattern_at(
        &mut self,
        pattern: &'c BindingPattern<'c>,
        source: Option<&'c Expression<'c>>,
        top: bool,
    ) -> Result<(), CompileError> {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                let resolved = self.resolve_symbol(symbol_of(ident)?)?;
                self.emit_initialize(resolved)
            }
            BindingPattern::AssignmentPattern(with_default) => {
                let name = match &with_default.left {
                    BindingPattern::BindingIdentifier(ident) => Some(self.intern_str(ident.name.as_str())),
                    _ => None,
                };
                self.emit_default(&with_default.right, name)?;
                self.bind_pattern_at(&with_default.left, Some(&with_default.right), top)
            }
            BindingPattern::ObjectPattern(object) => {
                if top {
                    let first = object.properties.first().map(|p| self.key_name(&p.key, p.computed));
                    self.emit_require_coercible(first)?;
                }
                for property in &object.properties {
                    self.builder().emit(Opcode::Dup)?;
                    self.emit_key_read(&property.key, property.computed)?;
                    self.bind_pattern_at(&property.value, None, false)?;
                }
                if let Some(rest) = &object.rest {
                    let excluded = self.static_keys(object.properties.iter().map(|p| (&p.key, p.computed)))?;
                    self.emit_rest_object(&excluded)?;
                    self.bind_pattern_at(&rest.argument, None, false)
                } else {
                    self.builder().emit(Opcode::Pop)
                }
            }
            BindingPattern::ArrayPattern(array) => {
                self.emit_get_iterator(source)?;
                for element in &array.elements {
                    self.builder().emit(Opcode::IteratorStep)?;
                    match element {
                        Some(element) => self.bind_pattern_at(element, None, false)?,
                        None => self.builder().emit(Opcode::Pop)?,
                    }
                }
                if let Some(rest) = &array.rest {
                    self.builder().emit(Opcode::IteratorRest)?;
                    self.bind_pattern_at(&rest.argument, None, false)?;
                }
                self.builder().emit(Opcode::IteratorClose)
            }
        }
    }

    /// Assigns the top value to an assignment target (`[a, b.c] = v`).
    pub(super) fn bind_target(&mut self, target: &'c AssignmentTarget<'c>) -> Result<(), CompileError> {
        match target {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                let resolved = self.resolve_reference(ident)?;
                self.emit_store(resolved)
            }
            AssignmentTarget::StaticMemberExpression(_)
            | AssignmentTarget::ComputedMemberExpression(_)
            | AssignmentTarget::PrivateFieldExpression(_) => {
                // `value` → `obj [key] value` → the stored value, dropped
                let member = target.to_member_expression();
                match self.compile_member_target(member)? {
                    Member::Static(atom) => {
                        self.builder().emit(Opcode::Swap)?;
                        self.builder().emit_u16(Opcode::SetProp, atom)?;
                    }
                    Member::Computed => {
                        self.builder().emit(Opcode::Rot3)?;
                        self.builder().emit(Opcode::Rot3)?;
                        self.builder().emit(Opcode::SetElem)?;
                    }
                }
                self.builder().emit(Opcode::Pop)
            }
            AssignmentTarget::ObjectAssignmentTarget(object) => {
                let first = object.properties.first().map(|p| match p {
                    AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(ident) => {
                        self.intern_str(ident.binding.name.as_str())
                    }
                    AssignmentTargetProperty::AssignmentTargetPropertyProperty(prop) => {
                        self.key_name(&prop.name, prop.computed)
                    }
                });
                self.emit_require_coercible(first)?;
                let mut excluded = Vec::new();
                for property in &object.properties {
                    self.builder().emit(Opcode::Dup)?;
                    match property {
                        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(ident) => {
                            let atom = self.intern_str(ident.binding.name.as_str());
                            excluded.push(atom);
                            self.builder().emit_u16(Opcode::GetProp, atom_operand(atom)?)?;
                            if let Some(init) = &ident.init {
                                self.emit_default(init, Some(atom))?;
                            }
                            let resolved = self.resolve_reference(&ident.binding)?;
                            self.emit_store(resolved)?;
                        }
                        AssignmentTargetProperty::AssignmentTargetPropertyProperty(prop) => {
                            if !prop.computed {
                                excluded.push(self.key_name(&prop.name, false));
                            }
                            self.emit_key_read(&prop.name, prop.computed)?;
                            self.bind_maybe_default(&prop.binding)?;
                        }
                    }
                }
                if let Some(rest) = &object.rest {
                    self.emit_rest_object(&excluded)?;
                    self.bind_target(&rest.target)
                } else {
                    self.builder().emit(Opcode::Pop)
                }
            }
            AssignmentTarget::ArrayAssignmentTarget(array) => {
                self.emit_get_iterator(None)?;
                for element in &array.elements {
                    self.builder().emit(Opcode::IteratorStep)?;
                    match element {
                        Some(element) => self.bind_maybe_default(element)?,
                        None => self.builder().emit(Opcode::Pop)?,
                    }
                }
                if let Some(rest) = &array.rest {
                    self.builder().emit(Opcode::IteratorRest)?;
                    self.bind_target(&rest.target)?;
                }
                self.builder().emit(Opcode::IteratorClose)
            }
            AssignmentTarget::TSAsExpression(_)
            | AssignmentTarget::TSSatisfiesExpression(_)
            | AssignmentTarget::TSNonNullExpression(_)
            | AssignmentTarget::TSTypeAssertion(_) => {
                Err(CompileError::unsupported("TypeScript syntax", Self::span_of(target)))
            }
        }
    }

    /// An assignment target with an optional default (`[a = 1] = v`).
    fn bind_maybe_default(&mut self, target: &'c AssignmentTargetMaybeDefault<'c>) -> Result<(), CompileError> {
        match target {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(with_default) => {
                let name = match &with_default.binding {
                    AssignmentTarget::AssignmentTargetIdentifier(ident) => Some(self.intern_str(ident.name.as_str())),
                    _ => None,
                };
                self.emit_default(&with_default.init, name)?;
                self.bind_target(&with_default.binding)
            }
            other => self.bind_target(other.to_assignment_target()),
        }
    }

    /// Replaces the top value with `default` when it is `undefined`.
    fn emit_default(&mut self, default: &'c Expression<'c>, name: Option<AtomId>) -> Result<(), CompileError> {
        self.builder().emit(Opcode::Dup)?;
        self.builder().emit(Opcode::LoadUndefined)?;
        self.builder().emit(Opcode::StrictEq)?;
        let keep = self.builder().emit_jump(Opcode::JumpIfFalse)?;
        self.builder().emit(Opcode::Pop)?;
        self.compile_expression_named(default, name)?;
        self.builder().patch_jump(keep)
    }

    /// `obj` → `obj value`: reads a pattern property's value.
    fn emit_key_read(&mut self, key: &'c PropertyKey<'c>, computed: bool) -> Result<(), CompileError> {
        if computed {
            self.compile_expression(key.to_expression())?;
            return self.builder().emit(Opcode::GetElem);
        }
        let atom = self.key_name(key, false);
        if let Some(index) = self.interns.atom(atom).as_array_index() {
            self.emit_number(f64::from(index))?;
            return self.builder().emit(Opcode::GetElem);
        }
        self.builder().emit_u16(Opcode::GetProp, atom_operand(atom)?)
    }

    /// The atom of a non-computed pattern key (an identifier or literal).
    fn key_name(&mut self, key: &'c PropertyKey<'c>, computed: bool) -> AtomId {
        if computed {
            return StaticAtom::Empty.id();
        }
        match key {
            PropertyKey::StaticIdentifier(ident) => self.intern_str(ident.name.as_str()),
            PropertyKey::StringLiteral(lit) => self.intern_units(lit.value.encode_utf16()),
            PropertyKey::NumericLiteral(lit) => {
                let text = number_to_string(lit.value);
                self.intern_str(&text)
            }
            _ => StaticAtom::Empty.id(),
        }
    }

    /// The names a rest pattern leaves out; a computed key is not supported there.
    fn static_keys(
        &mut self,
        keys: impl Iterator<Item = (&'c PropertyKey<'c>, bool)>,
    ) -> Result<Vec<AtomId>, CompileError> {
        keys.map(|(key, computed)| {
            if computed {
                Err(CompileError::unsupported(
                    "a computed key before a rest pattern",
                    Self::span_of(key),
                ))
            } else {
                Ok(self.key_name(key, false))
            }
        })
        .collect()
    }

    /// `value` → `rest`: a new object with `value`'s own enumerable properties
    /// minus `excluded`.
    fn emit_rest_object(&mut self, excluded: &[AtomId]) -> Result<(), CompileError> {
        self.builder().emit(Opcode::NewObject)?;
        self.builder().emit(Opcode::Swap)?;
        self.builder().emit(Opcode::CopyDataProperties)?;
        for atom in excluded {
            self.builder().emit(Opcode::Dup)?;
            if let Some(index) = self.interns.atom(*atom).as_array_index() {
                self.emit_number(f64::from(index))?;
                self.builder().emit(Opcode::DeleteElem)?;
            } else {
                self.builder().emit_u16(Opcode::DeleteProp, atom_operand(*atom)?)?;
            }
            self.builder().emit(Opcode::Pop)?;
        }
        Ok(())
    }

    fn emit_require_coercible(&mut self, first_key: Option<AtomId>) -> Result<(), CompileError> {
        let atom = first_key.unwrap_or(StaticAtom::Empty.id());
        self.builder()
            .emit_u16(Opcode::RequireObjectCoercible, atom_operand(atom)?)
    }

    /// `GetIterator` naming the initializer when the pattern has one.
    fn emit_get_iterator(&mut self, source: Option<&'c Expression<'c>>) -> Result<(), CompileError> {
        let atom = match source {
            Some(expression) => self.callee_text(expression),
            None => StaticAtom::Empty.id(),
        };
        self.builder().emit_u16(Opcode::GetIterator, atom_operand(atom)?)
    }
}
