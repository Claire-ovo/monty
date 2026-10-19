//! Class bodies: the constructor, methods and accessors on the prototype or
//! the constructor, static fields and blocks run at definition time, and
//! instance fields collected into one initializer the constructor runs.

use oxc_ast::ast::{Class, ClassElement, Expression, MethodDefinitionKind, PropertyKey, StaticBlock};
use oxc_span::Span;
use oxc_syntax::scope::ScopeId;

use super::{Compiler, atom_operand, scope_of, symbol_of};
use crate::{
    bytecode::Opcode,
    error::CompileError,
    function::FunctionKind,
    intern::{AtomId, FunctionId, StaticAtom},
    vm::number::number_to_string,
};

/// `DefineAccessor` flag: the function is a setter.
const ACCESSOR_SETTER: u8 = 1;
/// `DefineAccessor` flag: the property is not enumerable (class accessors).
const ACCESSOR_HIDDEN: u8 = 2;

impl<'c> Compiler<'c> {
    /// Compiles a class, leaving the constructor on the stack. The class's
    /// name binding (the class scope's own for an expression, the enclosing
    /// scope's for a declaration) is initialized before the static elements
    /// run, so they can refer to the class.
    pub(super) fn compile_class(&mut self, class: &'c Class<'c>, name: Option<AtomId>) -> Result<(), CompileError> {
        if !class.decorators.is_empty() {
            return Err(CompileError::unsupported("decorators", Self::span_of(class)));
        }
        let scope = scope_of(&class.scope_id)?;
        let name = class
            .id
            .as_ref()
            .map(|id| self.intern_str(id.name.as_str()))
            .or(name)
            .unwrap_or(StaticAtom::Empty.id());
        self.declare_scope(scope, false)?;
        // heritage, then the constructor, then `MakeClass` joins them
        let parent = class.heritage.as_ref().map(|heritage| &heritage.expression);
        let derived = parent.is_some();
        match parent {
            Some(parent) => self.compile_expression(parent)?,
            None => self.builder().emit(Opcode::LoadHole)?,
        }
        let constructor = class.body.body.iter().find_map(|element| match element {
            ClassElement::MethodDefinition(method) if method.kind == MethodDefinitionKind::Constructor => {
                Some(&**method)
            }
            _ => None,
        });
        let kind = FunctionKind::ClassConstructor { derived };
        let constructor_id = match constructor {
            Some(method) => self.compile_function_as(&method.value, name, kind)?,
            None => self.compile_default_constructor(scope, name, class.span, derived)?,
        };
        self.emit_make_closure(constructor_id)?;
        self.locate(class.span);
        self.builder().emit(Opcode::MakeClass)?;
        // the class's name binding, visible to its methods and static elements
        if let Some(id) = &class.id {
            self.builder().emit(Opcode::Dup)?;
            let resolved = self.resolve_symbol(symbol_of(id)?)?;
            self.emit_initialize(resolved)?;
        }
        // methods and accessors, in order
        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method) if method.kind != MethodDefinitionKind::Constructor => {
                    self.locate(method.span);
                    self.push_method_home(method.r#static)?;
                    let key = self.class_key(&method.key, method.computed)?;
                    let method_name = key.name_hint();
                    let function_kind = FunctionKind::Method;
                    let id = self.compile_function_as(&method.value, method_name, function_kind)?;
                    self.emit_method(id)?;
                    match method.kind {
                        MethodDefinitionKind::Method => match key {
                            ClassKey::Static(atom) => self.builder().emit_u16(Opcode::DefineMethod, atom)?,
                            ClassKey::Index(index) => {
                                self.emit_number(f64::from(index))?;
                                self.builder().emit(Opcode::Swap)?;
                                self.builder().emit(Opcode::DefineMethodElem)?;
                            }
                            ClassKey::Computed(expr) => {
                                self.compile_expression(expr)?;
                                self.builder().emit(Opcode::ToPropertyKey)?;
                                self.builder().emit(Opcode::Swap)?;
                                self.builder().emit(Opcode::DefineMethodElem)?;
                            }
                        },
                        MethodDefinitionKind::Get | MethodDefinitionKind::Set => {
                            match key {
                                ClassKey::Static(atom) => self.emit_atom(AtomId::from_u16(atom))?,
                                ClassKey::Index(index) => self.emit_number(f64::from(index))?,
                                ClassKey::Computed(expr) => self.compile_expression(expr)?,
                            }
                            self.builder().emit(Opcode::Swap)?;
                            let mut flags = ACCESSOR_HIDDEN;
                            if method.kind == MethodDefinitionKind::Set {
                                flags |= ACCESSOR_SETTER;
                            }
                            self.builder().emit_u8(Opcode::DefineAccessor, flags)?;
                        }
                        MethodDefinitionKind::Constructor => unreachable!("filtered above"),
                    }
                    // `class proto` → `class`
                    self.builder().emit(Opcode::Pop)?;
                }
                ClassElement::PropertyDefinition(property) if property.r#static => {
                    self.compile_static_field(
                        scope,
                        property.span,
                        &property.key,
                        property.computed,
                        property.value.as_ref(),
                    )?;
                }
                ClassElement::StaticBlock(block) => self.compile_static_block(block)?,
                ClassElement::MethodDefinition(_) | ClassElement::PropertyDefinition(_) => {}
                ClassElement::AccessorProperty(accessor) => {
                    return Err(CompileError::unsupported(
                        "accessor properties",
                        Self::span_of(&**accessor),
                    ));
                }
                ClassElement::TSIndexSignature(signature) => {
                    return Err(CompileError::unsupported(
                        "TypeScript syntax",
                        Self::span_of(&**signature),
                    ));
                }
            }
        }
        // instance fields: one initializer the constructor runs on each instance
        let fields: Vec<_> = class
            .body
            .body
            .iter()
            .filter_map(|element| match element {
                ClassElement::PropertyDefinition(property) if !property.r#static => Some(&**property),
                _ => None,
            })
            .collect();
        if !fields.is_empty() {
            self.builder().emit(Opcode::Dup)?;
            self.builder()
                .emit_u16(Opcode::GetProp, atom_operand(StaticAtom::Prototype.id())?)?;
            self.push_function(FunctionKind::Method, scope, StaticAtom::Empty.id(), class.span);
            self.bind_implicit_slots(scope)?;
            for field in fields {
                if let PropertyKey::PrivateIdentifier(_) = &field.key {
                    return Err(CompileError::unsupported("private fields", Self::span_of(field)));
                }
                self.locate(field.span);
                self.emit_this()?;
                let key = self.class_key(&field.key, field.computed)?;
                self.emit_field_value(field.value.as_ref(), &key)?;
                self.emit_define_field(&key)?;
                self.builder().emit(Opcode::Pop)?;
            }
            self.builder().emit(Opcode::ReturnUndefined)?;
            let function = self.pop_function();
            let id = self.publish_function(function)?;
            self.emit_method(id)?;
            // `class proto fn` → `class fn` → `class`
            self.builder().emit(Opcode::Swap)?;
            self.builder().emit(Opcode::Pop)?;
            self.builder().emit(Opcode::SetFieldInitializer)?;
        }
        Ok(())
    }

    /// The constructor a class without one gets: empty, or forwarding every
    /// argument to `super`.
    fn compile_default_constructor(
        &mut self,
        scope: ScopeId,
        name: AtomId,
        span: Span,
        derived: bool,
    ) -> Result<FunctionId, CompileError> {
        let kind = FunctionKind::ClassConstructor { derived };
        self.push_function(kind, scope, name, span);
        if derived {
            // slot 0 holds the rest array of every argument
            let slot = self.alloc_slot()?;
            self.func().formal_count = 1;
            self.func().has_rest = true;
            self.bind_implicit_slots(scope)?;
            self.builder().emit_u16(Opcode::LoadLocal, slot)?;
            self.builder().emit(Opcode::SuperCallSpread)?;
            self.builder().emit(Opcode::Pop)?;
        } else {
            self.bind_implicit_slots(scope)?;
        }
        self.builder().emit(Opcode::ReturnUndefined)?;
        let function = self.pop_function();
        self.publish_function(function)
    }

    /// Pushes the object a method is defined on: the class for a static
    /// member, else its prototype (`class` → `class home`).
    fn push_method_home(&mut self, is_static: bool) -> Result<(), CompileError> {
        self.builder().emit(Opcode::Dup)?;
        if !is_static {
            self.builder()
                .emit_u16(Opcode::GetProp, atom_operand(StaticAtom::Prototype.id())?)?;
        }
        Ok(())
    }

    /// A static field: its initializer runs as a method of the class (so
    /// `this` is the class) and defines the property on it.
    fn compile_static_field(
        &mut self,
        scope: ScopeId,
        span: Span,
        key: &'c PropertyKey<'c>,
        computed: bool,
        value: Option<&'c Expression<'c>>,
    ) -> Result<(), CompileError> {
        if let PropertyKey::PrivateIdentifier(_) = key {
            return Err(CompileError::unsupported("private fields", Self::span_of(key)));
        }
        self.builder().emit(Opcode::Dup)?;
        self.push_function(FunctionKind::Method, scope, StaticAtom::Empty.id(), span);
        self.bind_implicit_slots(scope)?;
        self.locate(span);
        self.emit_this()?;
        let key = self.class_key(key, computed)?;
        self.emit_field_value(value, &key)?;
        self.emit_define_field(&key)?;
        self.builder().emit(Opcode::Return)?;
        let function = self.pop_function();
        let id = self.publish_function(function)?;
        self.emit_static_init(id)
    }

    /// A `static { … }` block, run as a method of the class.
    fn compile_static_block(&mut self, block: &'c StaticBlock<'c>) -> Result<(), CompileError> {
        let scope = scope_of(&block.scope_id)?;
        self.builder().emit(Opcode::Dup)?;
        self.push_function(FunctionKind::Method, scope, StaticAtom::Empty.id(), block.span);
        self.bind_implicit_slots(scope)?;
        self.declare_scope(scope, true)?;
        self.hoist_functions(&block.body)?;
        self.compile_statements(&block.body)?;
        if !self.builder().is_dead() {
            self.builder().emit(Opcode::ReturnUndefined)?;
        }
        let function = self.pop_function();
        let id = self.publish_function(function)?;
        self.emit_static_init(id)
    }

    /// `class class` → `class`: calls the static initializer `id` with the
    /// class as `this` and the class as its home object.
    fn emit_static_init(&mut self, id: FunctionId) -> Result<(), CompileError> {
        self.emit_method(id)?;
        // `class class fn` → `class fn class` → `class result` → `class`
        self.builder().emit(Opcode::Swap)?;
        self.builder().emit_u8(Opcode::Call, 0)?;
        self.builder().emit(Opcode::Pop)
    }

    /// Pushes a field's value (`undefined` without an initializer), naming an
    /// anonymous function after the field.
    fn emit_field_value(&mut self, value: Option<&'c Expression<'c>>, key: &ClassKey<'c>) -> Result<(), CompileError> {
        match value {
            Some(value) => self.compile_expression_named(value, Some(key.name_hint())),
            None => self.builder().emit(Opcode::LoadUndefined),
        }
    }

    /// `this value` → `this`: defines a field as an own enumerable property.
    fn emit_define_field(&mut self, key: &ClassKey<'c>) -> Result<(), CompileError> {
        match *key {
            ClassKey::Static(atom) => self.builder().emit_u16(Opcode::DefineOwn, atom),
            ClassKey::Index(index) => {
                self.emit_number(f64::from(index))?;
                self.builder().emit(Opcode::Swap)?;
                self.builder().emit(Opcode::DefineOwnElem)
            }
            ClassKey::Computed(expr) => {
                // evaluated when the field is initialized rather than when the class is defined
                self.compile_expression(expr)?;
                self.builder().emit(Opcode::ToPropertyKey)?;
                self.builder().emit(Opcode::Swap)?;
                self.builder().emit(Opcode::DefineOwnElem)
            }
        }
    }

    /// Classifies a class element's key.
    fn class_key(&mut self, key: &'c PropertyKey<'c>, computed: bool) -> Result<ClassKey<'c>, CompileError> {
        if computed {
            return Ok(ClassKey::Computed(key.to_expression()));
        }
        let atom = match key {
            PropertyKey::StaticIdentifier(ident) => self.intern_str(ident.name.as_str()),
            PropertyKey::StringLiteral(lit) => self.intern_units(lit.value.encode_utf16()),
            PropertyKey::NumericLiteral(lit) => {
                let text = number_to_string(lit.value);
                self.intern_str(&text)
            }
            PropertyKey::PrivateIdentifier(_) => {
                return Err(CompileError::unsupported("private fields", Self::span_of(key)));
            }
            other => return Ok(ClassKey::Computed(other.to_expression())),
        };
        if let Some(index) = self.interns.atom(atom).as_array_index() {
            return Ok(ClassKey::Index(index));
        }
        Ok(ClassKey::Static(atom_operand(atom)?))
    }
}

/// A class element's key.
enum ClassKey<'c> {
    Static(u16),
    Index(u32),
    Computed(&'c Expression<'c>),
}

impl ClassKey<'_> {
    /// The name a member defined under this key gets.
    fn name_hint(&self) -> AtomId {
        match self {
            Self::Static(atom) => AtomId::from_u16(*atom),
            Self::Index(_) | Self::Computed(_) => StaticAtom::Empty.id(),
        }
    }
}
