//! Statement emission: control flow, declarations, loops and exceptions.
//!
//! `finally` blocks are inlined on every exit path (normal completion,
//! `break`/`continue`/`return` crossing them, and a catch-all handler that
//! rethrows), so the VM needs no completion records.

use std::mem;

use oxc_ast::ast::{
    AssignmentTarget, BlockStatement, ForStatement, ForStatementInit, ForStatementLeft, Program, Statement,
    SwitchStatement, TryStatement, VariableDeclaration, VariableDeclarationKind,
};
use oxc_span::GetSpan;
use oxc_syntax::scope::ScopeId;

use super::{Compiler, Control, binding_identifier, scope_of, symbol_of};
use crate::{
    bytecode::{Opcode, Target},
    error::CompileError,
};

/// Which statement a `break` or `continue` leaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit {
    Break,
    Continue,
}

impl<'c> Compiler<'c> {
    /// Compiles the module body, keeping the last top-level expression
    /// statement's value as the feed's completion value.
    pub(super) fn compile_module_statements(&mut self, program: &'c Program<'c>) -> Result<(), CompileError> {
        let completion = self.alloc_slot()?;
        let scope = self.module_scope();
        self.declare_scope(scope, true)?;
        self.hoist_functions(&program.body)?;
        for statement in &program.body {
            if self.builder().is_dead() {
                break;
            }
            if let Statement::ExpressionStatement(expression) = statement {
                self.compile_expression(&expression.expression)?;
                self.builder().emit_u16(Opcode::StoreLocal, completion)?;
            } else {
                self.compile_statement(statement, None)?;
            }
        }
        if !self.builder().is_dead() {
            self.builder().emit_u16(Opcode::LoadLocal, completion)?;
            self.builder().emit(Opcode::Return)?;
        }
        Ok(())
    }

    /// Compiles statements in order, stopping at unreachable code.
    pub(super) fn compile_statements(&mut self, statements: &'c [Statement<'c>]) -> Result<(), CompileError> {
        for statement in statements {
            if self.builder().is_dead() {
                break;
            }
            self.compile_statement(statement, None)?;
        }
        Ok(())
    }

    /// Compiles one statement; `label` is the label a `LabeledStatement` gave it.
    fn compile_statement(&mut self, statement: &'c Statement<'c>, label: Option<&'c str>) -> Result<(), CompileError> {
        self.locate(statement.span());
        match statement {
            Statement::ExpressionStatement(expression) => {
                self.compile_expression(&expression.expression)?;
                self.builder().emit(Opcode::Pop)
            }
            Statement::VariableDeclaration(declaration) => self.compile_variable_declaration(declaration),
            Statement::FunctionDeclaration(_) => Ok(()), // hoisted at scope entry
            Statement::BlockStatement(block) => self.compile_block(block),
            Statement::EmptyStatement(_) => Ok(()),
            Statement::DebuggerStatement(_) => Ok(()),
            Statement::IfStatement(statement) => {
                self.compile_expression(&statement.test)?;
                let otherwise = self.builder().emit_jump(Opcode::JumpIfFalse)?;
                self.compile_statement(&statement.consequent, None)?;
                if let Some(alternate) = &statement.alternate {
                    let end = self.builder().emit_jump(Opcode::Jump)?;
                    self.builder().patch_jump(otherwise)?;
                    self.compile_statement(alternate, None)?;
                    self.builder().patch_jump(end)
                } else {
                    self.builder().patch_jump(otherwise)
                }
            }
            Statement::WhileStatement(statement) => {
                let top = self.builder().target()?;
                self.compile_expression(&statement.test)?;
                let exit = self.builder().emit_jump(Opcode::JumpIfFalse)?;
                self.push_loop(label, false);
                self.compile_statement(&statement.body, None)?;
                self.finish_loop_iteration(top)?;
                self.builder().patch_jump(exit)?;
                self.pop_loop()
            }
            Statement::DoWhileStatement(statement) => {
                let top = self.builder().target()?;
                self.push_loop(label, false);
                self.compile_statement(&statement.body, None)?;
                self.patch_continues()?;
                self.compile_expression(&statement.test)?;
                self.builder().emit_jump_to(Opcode::JumpIfTrue, top)?;
                self.pop_loop()
            }
            Statement::ForStatement(statement) => self.compile_for(statement, label),
            Statement::ForOfStatement(statement) => {
                if statement.r#await {
                    return Err(CompileError::unsupported("for await", Self::span_of(&**statement)));
                }
                let scope = scope_of(&statement.scope_id)?;
                self.compile_expression(&statement.right)?;
                self.builder().emit(Opcode::GetIterator)?;
                self.compile_iteration_loop(scope, &statement.left, &statement.body, label)
            }
            Statement::ForInStatement(statement) => {
                let scope = scope_of(&statement.scope_id)?;
                self.compile_expression(&statement.right)?;
                self.builder().emit(Opcode::ForInStart)?;
                self.compile_iteration_loop(scope, &statement.left, &statement.body, label)
            }
            Statement::BreakStatement(statement) => self.compile_exit(
                Exit::Break,
                statement.label.as_ref().map(|l| l.name.as_str()),
                statement.span,
            ),
            Statement::ContinueStatement(statement) => self.compile_exit(
                Exit::Continue,
                statement.label.as_ref().map(|l| l.name.as_str()),
                statement.span,
            ),
            Statement::ReturnStatement(statement) => {
                match &statement.argument {
                    Some(argument) => self.compile_expression(argument)?,
                    None => self.builder().emit(Opcode::LoadUndefined)?,
                }
                self.emit_return()
            }
            Statement::ThrowStatement(statement) => {
                self.compile_expression(&statement.argument)?;
                self.locate(statement.span);
                self.builder().emit(Opcode::Throw)
            }
            Statement::TryStatement(statement) => self.compile_try(statement),
            Statement::SwitchStatement(statement) => self.compile_switch(statement, label),
            Statement::LabeledStatement(statement) => {
                let name = statement.label.name.as_str();
                match &statement.body {
                    Statement::WhileStatement(_)
                    | Statement::DoWhileStatement(_)
                    | Statement::ForStatement(_)
                    | Statement::ForOfStatement(_)
                    | Statement::ForInStatement(_)
                    | Statement::SwitchStatement(_) => self.compile_statement(&statement.body, Some(name)),
                    body => {
                        self.func().controls.push(Control::Label {
                            label: name,
                            breaks: Vec::new(),
                        });
                        self.compile_statement(body, None)?;
                        let Some(Control::Label { breaks, .. }) = self.func().controls.pop() else {
                            return Err(CompileError::internal("label context mismatch"));
                        };
                        for jump in breaks {
                            self.builder().patch_jump(jump)?;
                        }
                        Ok(())
                    }
                }
            }
            Statement::ClassDeclaration(class) => Err(CompileError::unsupported("classes", Self::span_of(&**class))),
            Statement::WithStatement(statement) => Err(CompileError::syntax(
                "'with' statements are not allowed in strict mode",
                Self::span_of(&**statement),
            )),
            Statement::ImportDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_) => Err(CompileError::unsupported(
                "module declarations",
                Self::span_of(statement),
            )),
            // TODO(ts-syntax): type-only declarations compile to nothing; enums and namespaces need emission.
            Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSExternalModuleDeclaration(_)
            | Statement::TSNamespaceDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_) => {
                Err(CompileError::unsupported("TypeScript syntax", Self::span_of(statement)))
            }
        }
    }

    /// A block: its own lexical scope, functions hoisted to its start.
    fn compile_block(&mut self, block: &'c BlockStatement<'c>) -> Result<(), CompileError> {
        let scope = scope_of(&block.scope_id)?;
        self.declare_scope(scope, false)?;
        self.hoist_functions(&block.body)?;
        self.compile_statements(&block.body)
    }

    fn compile_variable_declaration(&mut self, declaration: &'c VariableDeclaration<'c>) -> Result<(), CompileError> {
        if matches!(
            declaration.kind,
            VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
        ) {
            return Err(CompileError::unsupported(
                "using declarations",
                Self::span_of(declaration),
            ));
        }
        for declarator in &declaration.declarations {
            let ident = binding_identifier(&declarator.id)?;
            let resolved = self.resolve_symbol(symbol_of(ident)?)?;
            match &declarator.init {
                Some(init) => self.compile_expression_named(init, Some(resolved.name))?,
                // `let x;` leaves the dead zone here; `var x;` changes nothing
                None if declaration.kind == VariableDeclarationKind::Var => continue,
                None => self.builder().emit(Opcode::LoadUndefined)?,
            }
            self.locate(declarator.span);
            self.emit_initialize(resolved)?;
        }
        Ok(())
    }

    // === Loops ===

    fn push_loop(&mut self, label: Option<&'c str>, iterator: bool) {
        self.func().controls.push(Control::Loop {
            label,
            breaks: Vec::new(),
            continues: Vec::new(),
            iterator,
        });
    }

    /// Binds pending `continue`s to the current position.
    fn patch_continues(&mut self) -> Result<(), CompileError> {
        let Some(Control::Loop { continues, .. }) = self.func().controls.last_mut() else {
            return Err(CompileError::internal("continue outside a loop context"));
        };
        let continues = mem::take(continues);
        for jump in continues {
            self.builder().patch_jump(jump)?;
        }
        Ok(())
    }

    /// Ends an iteration: `continue`s land here, then jump back to `top`.
    fn finish_loop_iteration(&mut self, top: Target) -> Result<(), CompileError> {
        self.patch_continues()?;
        if !self.builder().is_dead() {
            self.builder().emit_jump_to(Opcode::Jump, top)?;
        }
        Ok(())
    }

    /// Pops a loop context, binding its `break`s to the current position.
    fn pop_loop(&mut self) -> Result<(), CompileError> {
        let Some(Control::Loop { breaks, .. }) = self.func().controls.pop() else {
            return Err(CompileError::internal("loop context mismatch"));
        };
        for jump in breaks {
            self.builder().patch_jump(jump)?;
        }
        Ok(())
    }

    /// `for (init; test; update) body`, with fresh cells per iteration for
    /// captured `let` bindings.
    fn compile_for(&mut self, statement: &'c ForStatement<'c>, label: Option<&'c str>) -> Result<(), CompileError> {
        let scope = scope_of(&statement.scope_id)?;
        self.declare_scope(scope, false)?;
        match &statement.init {
            Some(ForStatementInit::VariableDeclaration(declaration)) => {
                self.compile_variable_declaration(declaration)?;
            }
            Some(init) => {
                self.compile_expression(init.to_expression())?;
                self.builder().emit(Opcode::Pop)?;
            }
            None => {}
        }
        let per_iteration: Vec<u16> = self.captured_slots(scope)?;
        let top = self.builder().target()?;
        let exit = match &statement.test {
            Some(test) => {
                self.compile_expression(test)?;
                Some(self.builder().emit_jump(Opcode::JumpIfFalse)?)
            }
            None => None,
        };
        self.push_loop(label, false);
        self.compile_statement(&statement.body, None)?;
        self.patch_continues()?;
        if !self.builder().is_dead() {
            for slot in per_iteration {
                self.builder().emit_u16(Opcode::CopyCell, slot)?;
            }
            if let Some(update) = &statement.update {
                self.compile_expression(update)?;
                self.builder().emit(Opcode::Pop)?;
            }
            self.builder().emit_jump_to(Opcode::Jump, top)?;
        }
        if let Some(exit) = exit {
            self.builder().patch_jump(exit)?;
        }
        self.pop_loop()
    }

    /// The cell slots of a scope's captured bindings.
    fn captured_slots(&mut self, scope: ScopeId) -> Result<Vec<u16>, CompileError> {
        let mut slots = Vec::new();
        for symbol in self.scope_symbols(scope) {
            if self.analysis.is_captured(symbol) {
                slots.push(self.declare_symbol(symbol)?.index);
            }
        }
        Ok(slots)
    }

    /// The shared shape of `for…of` and `for…in`: the iterator is on the
    /// stack; each iteration binds the next value and runs the body.
    fn compile_iteration_loop(
        &mut self,
        scope: ScopeId,
        left: &'c ForStatementLeft<'c>,
        body: &'c Statement<'c>,
        label: Option<&'c str>,
    ) -> Result<(), CompileError> {
        let top = self.builder().target()?;
        let exit = self.builder().emit_jump(Opcode::IteratorNext)?;
        // a fresh binding (and cell) per iteration
        self.declare_scope(scope, false)?;
        match left {
            ForStatementLeft::VariableDeclaration(declaration) => {
                let declarator = declaration
                    .declarations
                    .first()
                    .ok_or(CompileError::internal("for-in/of without a declarator"))?;
                let ident = binding_identifier(&declarator.id)?;
                let resolved = self.resolve_symbol(symbol_of(ident)?)?;
                self.emit_initialize(resolved)?;
            }
            left => self.store_to_target(left.to_assignment_target())?,
        }
        self.push_loop(label, true);
        self.compile_statement(body, None)?;
        self.finish_loop_iteration(top)?;
        self.builder().patch_jump(exit)?;
        self.pop_loop()
    }

    /// Stores the top value into an assignment target (a `for…of` left side).
    fn store_to_target(&mut self, target: &'c AssignmentTarget<'c>) -> Result<(), CompileError> {
        match target {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                let symbol = ident
                    .reference_id
                    .get()
                    .and_then(|id| self.scoping().get_reference(id).symbol_id());
                let resolved = match symbol {
                    Some(symbol) => self.resolve_symbol(symbol)?,
                    None => super::Resolved {
                        place: super::Place::Global,
                        name: self.intern_str(ident.name.as_str()),
                        lexical: false,
                        is_const: false,
                    },
                };
                self.emit_store(resolved)
            }
            other => Err(CompileError::unsupported("this loop target", Self::span_of(other))),
        }
    }

    // === break / continue / return ===

    /// Leaves enclosing statements up to the target, running `finally`
    /// blocks and releasing iterators on the way.
    fn compile_exit(&mut self, exit: Exit, label: Option<&str>, span: oxc_span::Span) -> Result<(), CompileError> {
        let target = self.find_exit_target(exit, label).ok_or_else(|| {
            let what = if exit == Exit::Break { "break" } else { "continue" };
            CompileError::syntax(format!("Illegal {what} statement"), (span.start, span.end))
        })?;
        // unwind what lies between here and the target, innermost first
        for index in (target + 1..self.func().controls.len()).rev() {
            match &self.func().controls[index] {
                Control::Finally { .. } => self.inline_finally(index)?,
                Control::Loop { iterator: true, .. } => self.builder().emit(Opcode::IteratorClose)?,
                Control::Switch { .. } => self.builder().emit(Opcode::Pop)?,
                Control::Loop { .. } | Control::Label { .. } => {}
            }
        }
        if exit == Exit::Break && matches!(self.func().controls[target], Control::Loop { iterator: true, .. }) {
            self.builder().emit(Opcode::IteratorClose)?;
        }
        let jump = self.builder().emit_jump(Opcode::Jump)?;
        match (&mut self.func().controls[target], exit) {
            (
                Control::Loop { breaks, .. } | Control::Switch { breaks, .. } | Control::Label { breaks, .. },
                Exit::Break,
            ) => {
                breaks.push(jump);
            }
            (Control::Loop { continues, .. }, Exit::Continue) => continues.push(jump),
            _ => return Err(CompileError::internal("exit target mismatch")),
        }
        Ok(())
    }

    /// The control context a `break`/`continue` targets.
    fn find_exit_target(&self, exit: Exit, label: Option<&str>) -> Option<usize> {
        self.func_ref()
            .controls
            .iter()
            .rposition(|control| match (control, exit) {
                (Control::Loop { label: own, .. }, _) => label.is_none_or(|l| *own == Some(l)),
                (Control::Switch { label: own, .. }, Exit::Break) => label.is_none_or(|l| *own == Some(l)),
                (Control::Label { label: own, .. }, Exit::Break) => label == Some(own),
                _ => false,
            })
    }

    fn func_ref(&self) -> &super::FunctionState<'c> {
        self.funcs.last().expect("a function is being compiled")
    }

    /// Compiles the `finally` body at `index` here, as code outside the
    /// contexts it protects.
    fn inline_finally(&mut self, index: usize) -> Result<(), CompileError> {
        let saved = self.func().controls.split_off(index);
        let Some(Control::Finally { body }) = saved.first() else {
            return Err(CompileError::internal("inlining a non-finally context"));
        };
        let body = *body;
        let result = self.compile_block(body);
        self.func().controls.extend(saved);
        result
    }

    /// Returns the top value, running enclosing `finally` blocks first.
    fn emit_return(&mut self) -> Result<(), CompileError> {
        for index in (0..self.func().controls.len()).rev() {
            match &self.func().controls[index] {
                Control::Finally { .. } => {
                    let scratch = self.scratch_slot()?;
                    self.builder().emit_u16(Opcode::StoreLocal, scratch)?;
                    self.inline_finally(index)?;
                    self.builder().emit_u16(Opcode::LoadLocal, scratch)?;
                }
                Control::Loop { iterator: true, .. } => {
                    self.builder().emit(Opcode::Swap)?;
                    self.builder().emit(Opcode::IteratorClose)?;
                }
                Control::Switch { .. } => {
                    self.builder().emit(Opcode::Swap)?;
                    self.builder().emit(Opcode::Pop)?;
                }
                Control::Loop { .. } | Control::Label { .. } => {}
            }
        }
        self.builder().emit(Opcode::Return)
    }

    // === try / switch ===

    fn compile_try(&mut self, statement: &'c TryStatement<'c>) -> Result<(), CompileError> {
        let depth = self
            .builder()
            .depth()
            .ok_or(CompileError::internal("try in unreachable code"))?;
        if let Some(finalizer) = &statement.finalizer {
            self.func().controls.push(Control::Finally { body: finalizer });
        }
        let try_start = self.builder().offset();
        self.compile_block(&statement.block)?;
        let try_end = self.builder().offset();
        let mut to_end = Vec::new();
        if !self.builder().is_dead() {
            if statement.finalizer.is_some() {
                let index = self.func().controls.len() - 1;
                self.inline_finally(index)?;
            }
            to_end.push(self.builder().emit_jump(Opcode::Jump)?);
        }
        let mut protected = vec![(try_start, try_end)];
        if let Some(handler) = &statement.handler {
            let catch_start = self.builder().offset();
            self.builder().resume_at(depth);
            self.builder().add_handler(try_start, try_end, catch_start, depth)?;
            self.builder().emit(Opcode::PushException)?;
            let scope = scope_of(&handler.scope_id)?;
            self.declare_scope(scope, false)?;
            match &handler.param {
                Some(param) => {
                    let ident = binding_identifier(&param.pattern)?;
                    let resolved = self.resolve_symbol(symbol_of(ident)?)?;
                    self.emit_initialize(resolved)?;
                }
                None => self.builder().emit(Opcode::Pop)?,
            }
            self.compile_block(&handler.body)?;
            let catch_end = self.builder().offset();
            protected.push((catch_start, catch_end));
            if !self.builder().is_dead() {
                if statement.finalizer.is_some() {
                    let index = self.func().controls.len() - 1;
                    self.inline_finally(index)?;
                }
                to_end.push(self.builder().emit_jump(Opcode::Jump)?);
            }
        }
        if statement.finalizer.is_some() {
            let Some(Control::Finally { body }) = self.func().controls.pop() else {
                return Err(CompileError::internal("finally context mismatch"));
            };
            // anything thrown in the try or catch: run the finally, then rethrow
            let handler = self.builder().offset();
            self.builder().resume_at(depth);
            for (start, end) in protected {
                if start != end {
                    self.builder().add_handler(start, end, handler, depth)?;
                }
            }
            let scratch = self.scratch_slot()?;
            self.builder().emit(Opcode::PushException)?;
            self.builder().emit_u16(Opcode::StoreLocal, scratch)?;
            self.compile_block(body)?;
            if !self.builder().is_dead() {
                self.builder().emit_u16(Opcode::LoadLocal, scratch)?;
                self.builder().emit(Opcode::Throw)?;
            }
        }
        for jump in to_end {
            self.builder().patch_jump(jump)?;
        }
        Ok(())
    }

    /// `switch`: the discriminant stays on the stack while the cases run.
    fn compile_switch(
        &mut self,
        statement: &'c SwitchStatement<'c>,
        label: Option<&'c str>,
    ) -> Result<(), CompileError> {
        self.compile_expression(&statement.discriminant)?;
        let scope = scope_of(&statement.scope_id)?;
        self.declare_scope(scope, false)?;
        // the cases share one scope, so every case's functions hoist before the tests run
        for case in &statement.cases {
            self.hoist_functions(&case.consequent)?;
        }
        let mut tests = Vec::with_capacity(statement.cases.len());
        for case in &statement.cases {
            if let Some(test) = &case.test {
                self.builder().emit(Opcode::Dup)?;
                self.compile_expression(test)?;
                self.builder().emit(Opcode::StrictEq)?;
                tests.push(Some(self.builder().emit_jump(Opcode::JumpIfTrue)?));
            } else {
                tests.push(None);
            }
        }
        let mut to_default = Some(self.builder().emit_jump(Opcode::Jump)?);
        self.func().controls.push(Control::Switch {
            label,
            breaks: Vec::new(),
        });
        for (case, test) in statement.cases.iter().zip(tests) {
            match test {
                Some(jump) => self.builder().patch_jump(jump)?,
                None => {
                    if let Some(jump) = to_default.take() {
                        self.builder().patch_jump(jump)?;
                    }
                }
            }
            self.compile_statements(&case.consequent)?;
        }
        let Some(Control::Switch { breaks, .. }) = self.func().controls.pop() else {
            return Err(CompileError::internal("switch context mismatch"));
        };
        if let Some(jump) = to_default {
            self.builder().patch_jump(jump)?;
        }
        for jump in breaks {
            self.builder().patch_jump(jump)?;
        }
        if !self.builder().is_dead() {
            self.builder().emit(Opcode::Pop)?;
        }
        Ok(())
    }
}
