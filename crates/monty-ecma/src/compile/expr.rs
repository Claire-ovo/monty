//! Expression emission: every expression leaves exactly one value.

use oxc_ast::ast::{
    Argument, ArrayExpressionElement, ArrowFunctionBody, ArrowFunctionExpression, AssignmentExpression,
    AssignmentOperator, AssignmentTarget, BinaryOperator, CallExpression, ChainElement, ChainExpression, Expression,
    Function, IdentifierReference, LogicalOperator, MemberExpression, NewExpression, ObjectExpression,
    ObjectPropertyKind, PropertyKey, PropertyKind, SimpleAssignmentTarget, TemplateLiteral, UnaryOperator,
    UpdateExpression, UpdateOperator,
};
use oxc_span::GetSpan;

use super::{Compiler, FunctionBody, FunctionSpec, Place, Resolved, atom_operand, scope_of, symbol_of};
use crate::{
    bytecode::Opcode,
    error::CompileError,
    function::FunctionKind,
    intern::{AtomId, FunctionId, StaticAtom},
    vm::number::number_to_string,
};

/// The object (and key) of a member expression, pushed for the operation that follows.
enum Member {
    /// `obj` is on the stack; the property name is this atom.
    Static(u16),
    /// `obj key` are on the stack.
    Computed,
}

impl<'c> Compiler<'c> {
    /// Compiles `expr`, leaving its value.
    pub(super) fn compile_expression(&mut self, expr: &'c Expression<'c>) -> Result<(), CompileError> {
        self.compile_expression_named(expr, None)
    }

    /// Compiles `expr` with the name an anonymous function or arrow receives
    /// (`NamedEvaluation`: `const f = () => {}` names the arrow `f`).
    pub(super) fn compile_expression_named(
        &mut self,
        expr: &'c Expression<'c>,
        name: Option<AtomId>,
    ) -> Result<(), CompileError> {
        self.locate(expr.span());
        match expr {
            Expression::BooleanLiteral(lit) => {
                let op = if lit.value { Opcode::LoadTrue } else { Opcode::LoadFalse };
                self.builder().emit(op)
            }
            Expression::NullLiteral(_) => self.builder().emit(Opcode::LoadNull),
            Expression::NumericLiteral(lit) => self.emit_number(lit.value),
            Expression::StringLiteral(lit) => {
                let atom = self.intern_units(lit.value.encode_utf16());
                self.emit_atom(atom)
            }
            Expression::TemplateLiteral(template) => self.compile_template(template),
            Expression::Identifier(ident) => self.compile_identifier(ident),
            Expression::ThisExpression(_) => match self.resolve_this()? {
                None => self.builder().emit(Opcode::LoadUndefined),
                Some(Place::Local(slot)) => self.builder().emit_u16(Opcode::LoadLocal, slot),
                Some(Place::Cell(slot)) => self.builder().emit_u16(Opcode::LoadCell, slot),
                Some(Place::Script(_) | Place::Global) => Err(CompileError::internal("this resolved to a global")),
            },
            Expression::ArrayExpression(array) => {
                let plain = !array.elements.iter().any(|element| {
                    matches!(
                        element,
                        ArrayExpressionElement::SpreadElement(_) | ArrayExpressionElement::Elision(_)
                    )
                });
                if plain && u16::try_from(array.elements.len()).is_ok() {
                    for element in &array.elements {
                        self.compile_expression(element.to_expression())?;
                    }
                    let count = u16::try_from(array.elements.len()).map_err(|_| CompileError::TooManyConstants)?;
                    self.builder().emit_u16(Opcode::NewArray, count)
                } else {
                    self.builder().emit_u16(Opcode::NewArray, 0)?;
                    for element in &array.elements {
                        match element {
                            ArrayExpressionElement::SpreadElement(spread) => {
                                self.compile_expression(&spread.argument)?;
                                self.builder().emit(Opcode::ArraySpread)?;
                            }
                            ArrayExpressionElement::Elision(_) => self.builder().emit(Opcode::ArrayPushHole)?,
                            other => {
                                self.compile_expression(other.to_expression())?;
                                self.builder().emit(Opcode::ArrayPush)?;
                            }
                        }
                    }
                    Ok(())
                }
            }
            Expression::ObjectExpression(object) => self.compile_object(object),
            Expression::FunctionExpression(function) => {
                let name = function
                    .id
                    .as_ref()
                    .map(|id| self.intern_str(id.name.as_str()))
                    .or(name)
                    .unwrap_or(StaticAtom::Empty.id());
                let id = self.compile_function_node(function, name)?;
                self.emit_make_closure(id)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                let id = self.compile_arrow(arrow, name.unwrap_or(StaticAtom::Empty.id()))?;
                self.emit_make_closure(id)
            }
            Expression::ParenthesizedExpression(inner) => self.compile_expression_named(&inner.expression, name),
            Expression::SequenceExpression(sequence) => {
                let last = sequence.expressions.len().saturating_sub(1);
                for (index, expr) in sequence.expressions.iter().enumerate() {
                    self.compile_expression(expr)?;
                    if index != last {
                        self.builder().emit(Opcode::Pop)?;
                    }
                }
                Ok(())
            }
            Expression::UnaryExpression(unary) => self.compile_unary(unary.operator, &unary.argument),
            Expression::UpdateExpression(update) => self.compile_update(update),
            Expression::BinaryExpression(binary) => {
                self.compile_expression(&binary.left)?;
                self.compile_expression(&binary.right)?;
                self.locate(binary.span);
                self.builder().emit(binary_opcode(binary.operator))
            }
            Expression::LogicalExpression(logical) => {
                self.compile_expression(&logical.left)?;
                let jump = match logical.operator {
                    LogicalOperator::Or => Opcode::JumpIfTrueKeep,
                    LogicalOperator::And => Opcode::JumpIfFalseKeep,
                    LogicalOperator::Coalesce => Opcode::JumpIfNotNullishKeep,
                };
                let end = self.builder().emit_jump(jump)?;
                self.compile_expression_named(&logical.right, name)?;
                self.builder().patch_jump(end)
            }
            Expression::ConditionalExpression(cond) => {
                self.compile_expression(&cond.test)?;
                let otherwise = self.builder().emit_jump(Opcode::JumpIfFalse)?;
                self.compile_expression_named(&cond.consequent, name)?;
                let end = self.builder().emit_jump(Opcode::Jump)?;
                self.builder().patch_jump(otherwise)?;
                self.compile_expression_named(&cond.alternate, name)?;
                self.builder().patch_jump(end)
            }
            Expression::AssignmentExpression(assignment) => self.compile_assignment(assignment),
            Expression::CallExpression(call) => self.compile_call(call),
            Expression::NewExpression(new) => self.compile_new(new),
            Expression::ChainExpression(chain) => self.compile_chain(chain),
            Expression::StaticMemberExpression(_)
            | Expression::ComputedMemberExpression(_)
            | Expression::PrivateFieldExpression(_) => {
                let member = expr.to_member_expression();
                match self.compile_member_target(member)? {
                    Member::Static(atom) => self.builder().emit_u16(Opcode::GetProp, atom),
                    Member::Computed => self.builder().emit(Opcode::GetElem),
                }
            }
            Expression::ClassExpression(class) => Err(CompileError::unsupported("classes", Self::span_of(&**class))),
            Expression::AwaitExpression(_) => Err(CompileError::unsupported("async functions", Self::span_of(expr))),
            Expression::YieldExpression(_) => Err(CompileError::unsupported("generators", Self::span_of(expr))),
            Expression::RegExpLiteral(_) => Err(CompileError::unsupported("regular expressions", Self::span_of(expr))),
            Expression::BigIntLiteral(_) => Err(CompileError::unsupported("BigInt", Self::span_of(expr))),
            Expression::TaggedTemplateExpression(_) => {
                Err(CompileError::unsupported("tagged templates", Self::span_of(expr)))
            }
            Expression::Super(_) => Err(CompileError::unsupported("super", Self::span_of(expr))),
            Expression::NewTarget(_) => Err(CompileError::unsupported("new.target", Self::span_of(expr))),
            Expression::ImportExpression(_) => Err(CompileError::unsupported("dynamic import", Self::span_of(expr))),
            Expression::ImportMeta(_) => Err(CompileError::unsupported("import.meta", Self::span_of(expr))),
            Expression::PrivateInExpression(_) => Err(CompileError::unsupported("private fields", Self::span_of(expr))),
            Expression::JSXElement(_) | Expression::JSXFragment(_) => {
                Err(CompileError::unsupported("JSX", Self::span_of(expr)))
            }
            // TODO(ts-syntax): type-only wrappers compile to their inner expression once TypeScript feeds are accepted.
            Expression::TSAsExpression(_)
            | Expression::TSSatisfiesExpression(_)
            | Expression::TSTypeAssertion(_)
            | Expression::TSNonNullExpression(_)
            | Expression::TSInstantiationExpression(_)
            | Expression::V8IntrinsicExpression(_) => {
                Err(CompileError::unsupported("TypeScript syntax", Self::span_of(expr)))
            }
        }
    }

    /// Reads a variable: a binding, an implicit `arguments`, or a global.
    fn compile_identifier(&mut self, ident: &'c IdentifierReference<'c>) -> Result<(), CompileError> {
        let symbol = ident
            .reference_id
            .get()
            .and_then(|id| self.scoping().get_reference(id).symbol_id());
        if let Some(symbol) = symbol {
            let resolved = self.resolve_symbol(symbol)?;
            return self.emit_load(resolved);
        }
        match ident.name.as_str() {
            "undefined" => self.builder().emit(Opcode::LoadUndefined),
            "arguments" => match self.resolve_arguments()? {
                Some(Place::Local(slot)) => self.builder().emit_u16(Opcode::LoadLocal, slot),
                Some(Place::Cell(slot)) => self.builder().emit_u16(Opcode::LoadCell, slot),
                Some(Place::Script(_) | Place::Global) => Err(CompileError::internal("arguments resolved to a global")),
                None => self.emit_global_load(ident, false),
            },
            _ => self.emit_global_load(ident, false),
        }
    }

    /// Reads an unbound name: an earlier feed's binding or a `globalThis`
    /// property, which `typeof` tolerates missing.
    fn emit_global_load(&mut self, ident: &IdentifierReference<'c>, tolerant: bool) -> Result<(), CompileError> {
        let resolved = self.resolve_unbound(ident.name.as_str());
        match resolved.place {
            Place::Global if tolerant => self
                .builder()
                .emit_u16(Opcode::LoadGlobalOrUndefined, atom_operand(resolved.name)?),
            _ => self.emit_load(resolved),
        }
    }

    /// Resolves an identifier used as an assignment target.
    fn resolve_reference(&mut self, ident: &IdentifierReference<'c>) -> Result<Resolved, CompileError> {
        let symbol = ident
            .reference_id
            .get()
            .and_then(|id| self.scoping().get_reference(id).symbol_id());
        match symbol {
            Some(symbol) => self.resolve_symbol(symbol),
            None => Ok(self.resolve_unbound(ident.name.as_str())),
        }
    }

    // === Functions ===

    /// Compiles a function declaration or expression.
    pub(super) fn compile_function_node(
        &mut self,
        function: &'c Function<'c>,
        name: AtomId,
    ) -> Result<FunctionId, CompileError> {
        if function.r#async || function.generator {
            let feature = if function.generator {
                "generators"
            } else {
                "async functions"
            };
            return Err(CompileError::unsupported(feature, Self::span_of(function)));
        }
        let body = function
            .body
            .as_ref()
            .ok_or_else(|| CompileError::unsupported("TypeScript syntax", Self::span_of(function)))?;
        let scope = scope_of(&function.scope_id)?;
        let self_symbol = match (function.is_expression(), &function.id) {
            (true, Some(id)) => Some(symbol_of(id)?),
            _ => None,
        };
        self.compile_function(FunctionSpec {
            kind: FunctionKind::Normal,
            scope,
            name,
            span: function.span,
            params: &function.params,
            body: FunctionBody::Block(&body.statements),
            self_symbol,
        })
    }

    fn compile_arrow(
        &mut self,
        arrow: &'c ArrowFunctionExpression<'c>,
        name: AtomId,
    ) -> Result<FunctionId, CompileError> {
        if arrow.r#async {
            return Err(CompileError::unsupported("async functions", Self::span_of(arrow)));
        }
        let scope = scope_of(&arrow.scope_id)?;
        let body = match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => FunctionBody::Block(&body.statements),
            other => FunctionBody::Expression(other.to_expression()),
        };
        self.compile_function(FunctionSpec {
            kind: FunctionKind::Arrow,
            scope,
            name,
            span: arrow.span,
            params: &arrow.params,
            body,
            self_symbol: None,
        })
    }

    // === Members and calls ===

    /// Pushes a member expression's object (and computed key).
    fn compile_member_target(&mut self, member: &'c MemberExpression<'c>) -> Result<Member, CompileError> {
        match member {
            MemberExpression::StaticMemberExpression(member) => {
                self.compile_expression(&member.object)?;
                self.emit_optional_link(member.optional, 1)?;
                self.locate(member.span);
                let atom = self.intern_str(member.property.name.as_str());
                Ok(Member::Static(atom_operand(atom)?))
            }
            MemberExpression::ComputedMemberExpression(member) => {
                self.compile_expression(&member.object)?;
                self.emit_optional_link(member.optional, 1)?;
                self.compile_expression(&member.expression)?;
                self.locate(member.span);
                Ok(Member::Computed)
            }
            MemberExpression::PrivateFieldExpression(member) => {
                Err(CompileError::unsupported("private fields", Self::span_of(&**member)))
            }
        }
    }

    /// `callee this args… Call n`, with the receiver of a method call.
    fn compile_call(&mut self, call: &'c CallExpression<'c>) -> Result<(), CompileError> {
        match &call.callee {
            Expression::StaticMemberExpression(_)
            | Expression::ComputedMemberExpression(_)
            | Expression::PrivateFieldExpression(_) => {
                let member = call.callee.to_member_expression();
                match self.compile_member_target(member)? {
                    Member::Static(atom) => self.builder().emit_u16(Opcode::GetPropKeep, atom)?,
                    Member::Computed => self.builder().emit(Opcode::GetElemKeep)?,
                }
                // `obj fn` → `fn obj`
                self.builder().emit(Opcode::Swap)?;
                self.emit_optional_link(call.optional, 2)?;
            }
            Expression::Super(_) => return Err(CompileError::unsupported("super", Self::span_of(call))),
            callee => {
                self.compile_expression(callee)?;
                self.emit_optional_link(call.optional, 1)?;
                self.builder().emit(Opcode::LoadUndefined)?;
            }
        }
        self.locate(call.span);
        let callee = self.callee_text(&call.callee);
        if has_spread(&call.arguments) {
            self.compile_spread_arguments(&call.arguments)?;
            self.builder().set_callee(callee);
            self.builder().emit(Opcode::CallSpread)
        } else {
            let count = self.compile_arguments(&call.arguments)?;
            self.builder().set_callee(callee);
            self.builder().emit_u8(Opcode::Call, count)
        }
    }

    fn compile_new(&mut self, new: &'c NewExpression<'c>) -> Result<(), CompileError> {
        self.compile_expression(&new.callee)?;
        self.locate(new.span);
        let callee = self.callee_text(&new.callee);
        if has_spread(&new.arguments) {
            self.compile_spread_arguments(&new.arguments)?;
            self.builder().set_callee(callee);
            self.builder().emit(Opcode::NewSpread)
        } else {
            let count = self.compile_arguments(&new.arguments)?;
            self.builder().set_callee(callee);
            self.builder().emit_u8(Opcode::New, count)
        }
    }

    /// How V8 names a callee in `x is not a function`: identifiers and member
    /// chains by their text, literals re-rendered, anything else as
    /// `(intermediate value)`.
    fn callee_text(&mut self, callee: &Expression<'c>) -> AtomId {
        let text = render_callee(callee);
        self.intern_str(&text)
    }

    /// Pushes plain arguments, returning their count.
    fn compile_arguments(&mut self, arguments: &'c [Argument<'c>]) -> Result<u8, CompileError> {
        let count = u8::try_from(arguments.len())
            .map_err(|_| CompileError::syntax("too many arguments (at most 255)", (0, 0)))?;
        for argument in arguments {
            self.compile_expression(argument.to_expression())?;
        }
        Ok(count)
    }

    /// Pushes arguments containing a spread as one array.
    fn compile_spread_arguments(&mut self, arguments: &'c [Argument<'c>]) -> Result<(), CompileError> {
        self.builder().emit_u16(Opcode::NewArray, 0)?;
        for argument in arguments {
            match argument {
                Argument::SpreadElement(spread) => {
                    self.compile_expression(&spread.argument)?;
                    self.builder().emit(Opcode::ArraySpread)?;
                }
                other => {
                    self.compile_expression(other.to_expression())?;
                    self.builder().emit(Opcode::ArrayPush)?;
                }
            }
        }
        Ok(())
    }

    /// An optional chain: `a?.b.c` short-circuits to `undefined` the moment a
    /// link's object is nullish.
    fn compile_chain(&mut self, chain: &'c ChainExpression<'c>) -> Result<(), CompileError> {
        self.func().chains.push(Vec::new());
        let result = match &chain.expression {
            ChainElement::CallExpression(call) => self.compile_call(call),
            ChainElement::TSNonNullExpression(_) => {
                Err(CompileError::unsupported("TypeScript syntax", Self::span_of(chain)))
            }
            member => {
                let member = member.to_member_expression();
                match self.compile_member_target(member)? {
                    Member::Static(atom) => self.builder().emit_u16(Opcode::GetProp, atom),
                    Member::Computed => self.builder().emit(Opcode::GetElem),
                }
            }
        };
        let exits = self.func().chains.pop().expect("chain pushed above");
        result?;
        if exits.is_empty() {
            return Ok(());
        }
        let done = self.builder().emit_jump(Opcode::Jump)?;
        // group the exits by how much they leave on the stack
        let mut pops: Vec<u16> = exits.iter().map(|(_, pops)| *pops).collect();
        pops.sort_unstable();
        pops.dedup();
        let mut exits = exits;
        let mut to_done = vec![done];
        for count in pops {
            let (here, rest): (Vec<_>, Vec<_>) = exits.into_iter().partition(|(_, p)| *p == count);
            exits = rest;
            for (label, _) in here {
                self.builder().patch_jump(label)?;
            }
            for _ in 0..count {
                self.builder().emit(Opcode::Pop)?;
            }
            self.builder().emit(Opcode::LoadUndefined)?;
            to_done.push(self.builder().emit_jump(Opcode::Jump)?);
        }
        for label in to_done {
            self.builder().patch_jump(label)?;
        }
        Ok(())
    }

    /// Emits the nullish check of an optional link, leaving `pops` operands
    /// for the chain's exit to discard.
    fn emit_optional_link(&mut self, optional: bool, pops: u16) -> Result<(), CompileError> {
        if !optional {
            return Ok(());
        }
        let label = self.builder().emit_jump(Opcode::JumpIfNullishKeep)?;
        self.func()
            .chains
            .last_mut()
            .ok_or(CompileError::internal("optional link outside a chain"))?
            .push((label, pops));
        Ok(())
    }

    // === Assignment ===

    fn compile_assignment(&mut self, assignment: &'c AssignmentExpression<'c>) -> Result<(), CompileError> {
        let name = match &assignment.left {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => Some(self.intern_str(ident.name.as_str())),
            _ => None,
        };
        match &assignment.left {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                let resolved = self.resolve_reference(ident)?;
                match assignment.operator {
                    AssignmentOperator::Assign => {
                        self.compile_expression_named(&assignment.right, name)?;
                    }
                    AssignmentOperator::LogicalOr
                    | AssignmentOperator::LogicalAnd
                    | AssignmentOperator::LogicalNullish => {
                        self.emit_load(resolved)?;
                        let end = self.builder().emit_jump(logical_assignment_jump(assignment.operator))?;
                        self.compile_expression_named(&assignment.right, name)?;
                        self.builder().emit(Opcode::Dup)?;
                        self.locate(assignment.span);
                        self.emit_store(resolved)?;
                        return self.builder().patch_jump(end);
                    }
                    operator => {
                        self.emit_load(resolved)?;
                        self.compile_expression(&assignment.right)?;
                        self.locate(assignment.span);
                        self.builder().emit(compound_opcode(operator))?;
                    }
                }
                self.builder().emit(Opcode::Dup)?;
                self.locate(assignment.span);
                self.emit_store(resolved)
            }
            AssignmentTarget::StaticMemberExpression(_)
            | AssignmentTarget::ComputedMemberExpression(_)
            | AssignmentTarget::PrivateFieldExpression(_) => {
                let member = assignment.left.to_member_expression();
                let target = self.compile_member_target(member)?;
                let (get_keep, set) = match target {
                    Member::Static(_) => (Opcode::GetPropKeep, Opcode::SetProp),
                    Member::Computed => (Opcode::GetElemKeep, Opcode::SetElem),
                };
                let atom = match target {
                    Member::Static(atom) => atom,
                    Member::Computed => 0,
                };
                // `obj [key]` on the stack
                match assignment.operator {
                    AssignmentOperator::Assign => {
                        self.compile_expression(&assignment.right)?;
                    }
                    AssignmentOperator::LogicalOr
                    | AssignmentOperator::LogicalAnd
                    | AssignmentOperator::LogicalNullish => {
                        self.emit_member_op(get_keep, atom)?;
                        // `obj [key] value`: keep the value and jump, or drop it and assign
                        let short = self.builder().emit_jump(logical_assignment_jump(assignment.operator))?;
                        self.compile_expression(&assignment.right)?;
                        self.locate(assignment.span);
                        self.emit_member_op(set, atom)?;
                        let done = self.builder().emit_jump(Opcode::Jump)?;
                        self.builder().patch_jump(short)?;
                        // drop the object (and key) under the kept value
                        if matches!(target, Member::Computed) {
                            self.builder().emit(Opcode::Rot3)?;
                            self.builder().emit(Opcode::Pop)?;
                        }
                        self.builder().emit(Opcode::Swap)?;
                        self.builder().emit(Opcode::Pop)?;
                        return self.builder().patch_jump(done);
                    }
                    operator => {
                        self.emit_member_op(get_keep, atom)?;
                        self.compile_expression(&assignment.right)?;
                        self.locate(assignment.span);
                        self.builder().emit(compound_opcode(operator))?;
                    }
                }
                self.locate(assignment.span);
                self.emit_member_op(set, atom)
            }
            AssignmentTarget::ArrayAssignmentTarget(_) | AssignmentTarget::ObjectAssignmentTarget(_) => Err(
                CompileError::unsupported("destructuring", Self::span_of(&assignment.left)),
            ),
            AssignmentTarget::TSAsExpression(_)
            | AssignmentTarget::TSSatisfiesExpression(_)
            | AssignmentTarget::TSNonNullExpression(_)
            | AssignmentTarget::TSTypeAssertion(_) => Err(CompileError::unsupported(
                "TypeScript syntax",
                Self::span_of(&assignment.left),
            )),
        }
    }

    /// Emits a property opcode with its atom operand when it takes one.
    fn emit_member_op(&mut self, op: Opcode, atom: u16) -> Result<(), CompileError> {
        match op {
            Opcode::GetPropKeep | Opcode::SetProp | Opcode::GetProp | Opcode::DeleteProp => {
                self.builder().emit_u16(op, atom)
            }
            _ => self.builder().emit(op),
        }
    }

    /// `++x`, `x--`, `obj.p++`, …
    fn compile_update(&mut self, update: &'c UpdateExpression<'c>) -> Result<(), CompileError> {
        let delta: i8 = match update.operator {
            UpdateOperator::Increment => 1,
            UpdateOperator::Decrement => -1,
        };
        match &update.argument {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
                let resolved = self.resolve_reference(ident)?;
                self.emit_load(resolved)?;
                if update.prefix {
                    self.builder().emit_i8(Opcode::IncDec, delta)?;
                    self.builder().emit(Opcode::Dup)?;
                } else {
                    self.builder().emit(Opcode::Plus)?;
                    self.builder().emit(Opcode::Dup)?;
                    self.builder().emit_i8(Opcode::IncDec, delta)?;
                }
                self.locate(update.span);
                self.emit_store(resolved)
            }
            SimpleAssignmentTarget::StaticMemberExpression(_)
            | SimpleAssignmentTarget::ComputedMemberExpression(_)
            | SimpleAssignmentTarget::PrivateFieldExpression(_) => {
                let member = update.argument.to_member_expression();
                let target = self.compile_member_target(member)?;
                let (get_keep, set, rot, atom) = match target {
                    Member::Static(atom) => (Opcode::GetPropKeep, Opcode::SetProp, Opcode::Rot3, atom),
                    Member::Computed => (Opcode::GetElemKeep, Opcode::SetElem, Opcode::Rot4, 0),
                };
                self.emit_member_op(get_keep, atom)?;
                // `obj [key] value` → `result obj [key] new`
                if update.prefix {
                    self.builder().emit_i8(Opcode::IncDec, delta)?;
                    self.builder().emit(Opcode::Dup)?;
                    self.builder().emit(rot)?;
                } else {
                    self.builder().emit(Opcode::Plus)?;
                    self.builder().emit(Opcode::Dup)?;
                    self.builder().emit(rot)?;
                    self.builder().emit_i8(Opcode::IncDec, delta)?;
                }
                self.locate(update.span);
                self.emit_member_op(set, atom)?;
                self.builder().emit(Opcode::Pop)
            }
            SimpleAssignmentTarget::TSAsExpression(_)
            | SimpleAssignmentTarget::TSSatisfiesExpression(_)
            | SimpleAssignmentTarget::TSNonNullExpression(_)
            | SimpleAssignmentTarget::TSTypeAssertion(_) => Err(CompileError::unsupported(
                "TypeScript syntax",
                Self::span_of(&update.argument),
            )),
        }
    }

    // === Operators ===

    fn compile_unary(&mut self, operator: UnaryOperator, argument: &'c Expression<'c>) -> Result<(), CompileError> {
        match operator {
            UnaryOperator::Typeof => {
                if let Expression::Identifier(ident) = argument
                    && ident
                        .reference_id
                        .get()
                        .is_none_or(|id| self.scoping().get_reference(id).symbol_id().is_none())
                    && !matches!(ident.name.as_str(), "undefined" | "arguments")
                {
                    self.emit_global_load(ident, true)?;
                } else {
                    self.compile_expression(argument)?;
                }
                self.builder().emit(Opcode::TypeOf)
            }
            UnaryOperator::Void => {
                self.compile_expression(argument)?;
                self.builder().emit(Opcode::Pop)?;
                self.builder().emit(Opcode::LoadUndefined)
            }
            UnaryOperator::Delete => match argument {
                Expression::StaticMemberExpression(_)
                | Expression::ComputedMemberExpression(_)
                | Expression::PrivateFieldExpression(_) => {
                    let member = argument.to_member_expression();
                    match self.compile_member_target(member)? {
                        Member::Static(atom) => self.builder().emit_u16(Opcode::DeleteProp, atom),
                        Member::Computed => self.builder().emit(Opcode::DeleteElem),
                    }
                }
                other => {
                    // deleting anything but a property evaluates it and yields `true`
                    self.compile_expression(other)?;
                    self.builder().emit(Opcode::Pop)?;
                    self.builder().emit(Opcode::LoadTrue)
                }
            },
            UnaryOperator::UnaryPlus
            | UnaryOperator::UnaryNegation
            | UnaryOperator::LogicalNot
            | UnaryOperator::BitwiseNot => {
                self.compile_expression(argument)?;
                let op = match operator {
                    UnaryOperator::UnaryPlus => Opcode::Plus,
                    UnaryOperator::UnaryNegation => Opcode::Neg,
                    UnaryOperator::LogicalNot => Opcode::Not,
                    _ => Opcode::BitNot,
                };
                self.builder().emit(op)
            }
        }
    }

    // === Literals ===

    fn compile_template(&mut self, template: &'c TemplateLiteral<'c>) -> Result<(), CompileError> {
        let mut pending: u8 = 0;
        let mut pushed_any = false;
        for (index, quasi) in template.quasis.iter().enumerate() {
            let cooked = quasi.value.cooked.ok_or_else(|| {
                CompileError::syntax("invalid escape sequence in template literal", Self::span_of(quasi))
            })?;
            if cooked.len_utf16() > 0 {
                let atom = self.intern_units(cooked.encode_utf16());
                self.emit_atom(atom)?;
                pending = self.bump_template(pending)?;
                pushed_any = true;
            }
            if let Some(expr) = template.expressions.get(index) {
                self.compile_expression(expr)?;
                pending = self.bump_template(pending)?;
                pushed_any = true;
            }
        }
        if !pushed_any {
            return self.emit_atom(StaticAtom::Empty.id());
        }
        self.builder().emit_u8(Opcode::TemplateConcat, pending)
    }

    /// Counts a pushed template piece, folding the pieces so far when the
    /// operand would overflow (concatenation is associative).
    fn bump_template(&mut self, pending: u8) -> Result<u8, CompileError> {
        if pending == u8::MAX {
            self.builder().emit_u8(Opcode::TemplateConcat, u8::MAX)?;
            Ok(1)
        } else {
            Ok(pending + 1)
        }
    }

    fn compile_object(&mut self, object: &'c ObjectExpression<'c>) -> Result<(), CompileError> {
        self.builder().emit(Opcode::NewObject)?;
        for property in &object.properties {
            match property {
                ObjectPropertyKind::SpreadProperty(spread) => {
                    self.compile_expression(&spread.argument)?;
                    self.builder().emit(Opcode::CopyDataProperties)?;
                }
                ObjectPropertyKind::ObjectProperty(property) => {
                    self.locate(property.span);
                    let proto_key = match &property.key {
                        PropertyKey::StaticIdentifier(id) => id.name.as_str() == "__proto__",
                        PropertyKey::StringLiteral(lit) => lit.value.as_str() == Some("__proto__"),
                        _ => false,
                    };
                    let is_proto = proto_key
                        && !property.computed
                        && !property.method
                        && !property.shorthand
                        && property.kind == PropertyKind::Init;
                    if is_proto {
                        self.compile_expression(&property.value)?;
                        self.builder().emit(Opcode::SetPrototype)?;
                        continue;
                    }
                    let key = self.literal_key(&property.key, property.computed)?;
                    match property.kind {
                        PropertyKind::Init if property.method => {
                            let function = method_function(&property.value)?;
                            let name = key.name_hint();
                            let id = self.compile_function_node(function, name)?;
                            self.emit_method(id)?;
                            match key {
                                LiteralKey::Static(atom) => self.builder().emit_u16(Opcode::DefineOwn, atom)?,
                                LiteralKey::Index(index) => {
                                    self.emit_number(f64::from(index))?;
                                    self.builder().emit(Opcode::Swap)?;
                                    self.builder().emit(Opcode::DefineOwnElem)?;
                                }
                                LiteralKey::Computed(expr) => {
                                    self.compile_expression(expr)?;
                                    self.builder().emit(Opcode::Swap)?;
                                    self.builder().emit(Opcode::DefineOwnElem)?;
                                }
                            }
                        }
                        PropertyKind::Init => match key {
                            LiteralKey::Static(atom) => {
                                self.compile_expression_named(&property.value, Some(AtomId::from_u16(atom)))?;
                                self.builder().emit_u16(Opcode::DefineOwn, atom)?;
                            }
                            LiteralKey::Index(index) => {
                                self.emit_number(f64::from(index))?;
                                self.compile_expression(&property.value)?;
                                self.builder().emit(Opcode::DefineOwnElem)?;
                            }
                            LiteralKey::Computed(expr) => {
                                self.compile_expression(expr)?;
                                self.builder().emit(Opcode::ToPropertyKey)?;
                                self.compile_expression(&property.value)?;
                                self.builder().emit(Opcode::DefineOwnElem)?;
                            }
                        },
                        PropertyKind::Get | PropertyKind::Set => {
                            let function = method_function(&property.value)?;
                            let name = key.name_hint();
                            let id = self.compile_function_node(function, name)?;
                            self.emit_method(id)?;
                            match key {
                                LiteralKey::Static(atom) => self.emit_atom(AtomId::from_u16(atom))?,
                                LiteralKey::Index(index) => self.emit_number(f64::from(index))?,
                                LiteralKey::Computed(expr) => self.compile_expression(expr)?,
                            }
                            self.builder().emit(Opcode::Swap)?;
                            let which = u8::from(property.kind == PropertyKind::Set);
                            self.builder().emit_u8(Opcode::DefineAccessor, which)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `MakeMethod` on the object at the top of the stack.
    fn emit_method(&mut self, id: FunctionId) -> Result<(), CompileError> {
        let operand = id.as_u16().ok_or(CompileError::TooManyConstants)?;
        self.builder().emit_u16(Opcode::MakeMethod, operand)
    }

    /// Classifies a literal's property key.
    fn literal_key(&mut self, key: &'c PropertyKey<'c>, computed: bool) -> Result<LiteralKey<'c>, CompileError> {
        if computed {
            return Ok(LiteralKey::Computed(key.to_expression()));
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
            other => return Ok(LiteralKey::Computed(other.to_expression())),
        };
        // `{ 1: x }` and `{ '1': x }` are the array index 1, which orders before string keys
        if let Some(index) = self.interns.atom(atom).as_array_index() {
            return Ok(LiteralKey::Index(index));
        }
        Ok(LiteralKey::Static(atom_operand(atom)?))
    }
}

/// A literal's property key: a known name, a canonical array index, or an
/// expression to evaluate.
enum LiteralKey<'c> {
    Static(u16),
    Index(u32),
    Computed(&'c Expression<'c>),
}

impl LiteralKey<'_> {
    /// The name a method or accessor defined under this key gets.
    fn name_hint(&self) -> AtomId {
        match self {
            Self::Static(atom) => AtomId::from_u16(*atom),
            Self::Index(_) | Self::Computed(_) => StaticAtom::Empty.id(),
        }
    }
}

/// The function of a method or accessor property, which the parser always
/// produces as a function expression.
fn method_function<'c>(value: &'c Expression<'c>) -> Result<&'c Function<'c>, CompileError> {
    match value {
        Expression::FunctionExpression(function) => Ok(function),
        _ => Err(CompileError::internal("method value is not a function")),
    }
}

/// The text V8's error messages use for a callee expression.
fn render_callee(expr: &Expression<'_>) -> String {
    match expr {
        Expression::Identifier(ident) => ident.name.to_string(),
        Expression::ThisExpression(_) => "this".to_owned(),
        Expression::Super(_) => "super".to_owned(),
        Expression::NullLiteral(_) => "null".to_owned(),
        Expression::BooleanLiteral(lit) => lit.value.to_string(),
        Expression::NumericLiteral(lit) => number_to_string(lit.value),
        Expression::StringLiteral(lit) => format!("\"{}\"", lit.value.as_str().unwrap_or_default()),
        Expression::ParenthesizedExpression(inner) => render_callee(&inner.expression),
        Expression::StaticMemberExpression(member) => {
            format!("{}.{}", render_callee(&member.object), member.property.name)
        }
        Expression::ComputedMemberExpression(member) => {
            let key = match &member.expression {
                key @ (Expression::Identifier(_) | Expression::StringLiteral(_) | Expression::NumericLiteral(_)) => {
                    render_callee(key)
                }
                _ => "...".to_owned(),
            };
            format!("{}[{key}]", render_callee(&member.object))
        }
        Expression::CallExpression(call) => format!("{}(...)", render_callee(&call.callee)),
        Expression::ObjectExpression(object) if object.properties.is_empty() => "{}".to_owned(),
        _ => "(intermediate value)".to_owned(),
    }
}

fn has_spread(arguments: &[Argument<'_>]) -> bool {
    arguments.iter().any(|a| matches!(a, Argument::SpreadElement(_)))
}

fn binary_opcode(operator: BinaryOperator) -> Opcode {
    match operator {
        BinaryOperator::Equality => Opcode::Eq,
        BinaryOperator::Inequality => Opcode::Ne,
        BinaryOperator::StrictEquality => Opcode::StrictEq,
        BinaryOperator::StrictInequality => Opcode::StrictNe,
        BinaryOperator::LessThan => Opcode::Lt,
        BinaryOperator::LessEqualThan => Opcode::Le,
        BinaryOperator::GreaterThan => Opcode::Gt,
        BinaryOperator::GreaterEqualThan => Opcode::Ge,
        BinaryOperator::Addition => Opcode::Add,
        BinaryOperator::Subtraction => Opcode::Sub,
        BinaryOperator::Multiplication => Opcode::Mul,
        BinaryOperator::Division => Opcode::Div,
        BinaryOperator::Remainder => Opcode::Mod,
        BinaryOperator::Exponential => Opcode::Exp,
        BinaryOperator::ShiftLeft => Opcode::Shl,
        BinaryOperator::ShiftRight => Opcode::Shr,
        BinaryOperator::ShiftRightZeroFill => Opcode::UShr,
        BinaryOperator::BitwiseOR => Opcode::BitOr,
        BinaryOperator::BitwiseXOR => Opcode::BitXor,
        BinaryOperator::BitwiseAnd => Opcode::BitAnd,
        BinaryOperator::In => Opcode::In,
        BinaryOperator::Instanceof => Opcode::InstanceOf,
    }
}

/// The arithmetic opcode of a compound assignment operator.
fn compound_opcode(operator: AssignmentOperator) -> Opcode {
    match operator {
        AssignmentOperator::Addition => Opcode::Add,
        AssignmentOperator::Subtraction => Opcode::Sub,
        AssignmentOperator::Multiplication => Opcode::Mul,
        AssignmentOperator::Division => Opcode::Div,
        AssignmentOperator::Remainder => Opcode::Mod,
        AssignmentOperator::Exponential => Opcode::Exp,
        AssignmentOperator::ShiftLeft => Opcode::Shl,
        AssignmentOperator::ShiftRight => Opcode::Shr,
        AssignmentOperator::ShiftRightZeroFill => Opcode::UShr,
        AssignmentOperator::BitwiseOR => Opcode::BitOr,
        AssignmentOperator::BitwiseXOR => Opcode::BitXor,
        AssignmentOperator::BitwiseAnd => Opcode::BitAnd,
        AssignmentOperator::Assign
        | AssignmentOperator::LogicalOr
        | AssignmentOperator::LogicalAnd
        | AssignmentOperator::LogicalNullish => unreachable!("not a compound arithmetic assignment"),
    }
}

/// The short-circuit jump of a logical assignment.
fn logical_assignment_jump(operator: AssignmentOperator) -> Opcode {
    match operator {
        AssignmentOperator::LogicalOr => Opcode::JumpIfTrueKeep,
        AssignmentOperator::LogicalAnd => Opcode::JumpIfFalseKeep,
        _ => Opcode::JumpIfNotNullishKeep,
    }
}
