//! Scope facts the compiler needs beyond what `oxc_semantic` binds.
//!
//! oxc resolves every identifier to a symbol and every symbol to a scope; this
//! pass derives what the bytecode shape depends on: which bindings are
//! captured by an inner function (so they live in heap cells) and which
//! functions need their `this`, `arguments` or `new.target` materialized or
//! captured by arrows inside them.

use std::cell::Cell;

use ahash::{AHashMap, AHashSet};
use oxc_ast::ast::{IdentifierReference, NewTarget, Program, ThisExpression};
use oxc_ast_visit::{Visit, walk};
use oxc_semantic::Scoping;
use oxc_syntax::{
    scope::{ScopeFlags, ScopeId},
    symbol::SymbolId,
};

/// The resolved program: oxc's binding tables plus the derived facts.
pub(crate) struct Analysis {
    pub scoping: Scoping,
    /// Symbols referenced from a function other than the one declaring them.
    pub captured: AHashSet<SymbolId>,
    /// Per non-arrow function scope (the module scope included).
    pub functions: AHashMap<ScopeId, FunctionFacts>,
}

/// What a non-arrow function's body does with its implicit bindings.
#[derive(Debug, Default, Clone, Copy)]
#[expect(clippy::struct_excessive_bools, reason = "independent facts, not a state machine")]
pub(crate) struct FunctionFacts {
    /// `this` is read somewhere in the body, arrows included.
    pub uses_this: bool,
    /// An arrow inside the body reads `this`, so the slot must be a cell.
    pub this_captured: bool,
    /// `arguments` is read in the body (or an arrow inside it).
    pub uses_arguments: bool,
    /// An arrow inside the body reads `arguments`, so the slot must be a cell.
    pub arguments_captured: bool,
    /// `new.target` is read in the body (or an arrow inside it).
    pub uses_new_target: bool,
}

impl Analysis {
    /// Derives the facts for an annotated program.
    pub fn new(program: &Program<'_>, scoping: Scoping) -> Self {
        let captured = captured_symbols(&scoping);
        let mut visitor = FactsVisitor {
            scoping: &scoping,
            functions: AHashMap::new(),
            scopes: Vec::new(),
        };
        visitor.visit_program(program);
        let functions = visitor.functions;
        Self {
            scoping,
            captured,
            functions,
        }
    }

    /// Whether `symbol` must live in a cell.
    pub fn is_captured(&self, symbol: SymbolId) -> bool {
        self.captured.contains(&symbol)
    }

    /// The facts of a non-arrow function scope; the default when nothing was recorded.
    pub fn function_facts(&self, scope: ScopeId) -> FunctionFacts {
        self.functions.get(&scope).copied().unwrap_or_default()
    }

    /// The nearest enclosing scope (`scope` included) that is a function or the module.
    pub fn function_scope(&self, scope: ScopeId) -> ScopeId {
        function_scope(&self.scoping, scope, true)
    }
}

/// Symbols read or written from inside a function nested in the one that declares them.
fn captured_symbols(scoping: &Scoping) -> AHashSet<SymbolId> {
    scoping
        .symbol_ids()
        .filter(|symbol| {
            let home = function_scope(scoping, scoping.symbol_scope_id(*symbol), true);
            scoping
                .get_resolved_references(*symbol)
                .any(|reference| function_scope(scoping, reference.scope_id(), true) != home)
        })
        .collect()
}

/// Walks up from `scope` to the nearest function or module scope. With
/// `arrows` false, arrow functions are skipped: they have no `this` of their own.
fn function_scope(scoping: &Scoping, scope: ScopeId, arrows: bool) -> ScopeId {
    let mut current = scope;
    loop {
        let flags = scoping.scope_flags(current);
        let is_boundary = flags.is_top() || (flags.is_function() && (arrows || !flags.is_arrow()));
        if is_boundary {
            return current;
        }
        current = scoping.scope_parent_id(current).expect("a non-root scope has a parent");
    }
}

/// Records `this` / `arguments` / `new.target` use against the owning function.
struct FactsVisitor<'s> {
    scoping: &'s Scoping,
    functions: AHashMap<ScopeId, FunctionFacts>,
    /// Scopes entered so far, innermost last.
    scopes: Vec<ScopeId>,
}

impl FactsVisitor<'_> {
    /// The non-arrow function owning the current position, and whether an
    /// arrow sits between it and the position.
    fn owner(&self) -> Option<(ScopeId, bool)> {
        let current = *self.scopes.last()?;
        let owner = function_scope(self.scoping, current, false);
        let through_arrow = function_scope(self.scoping, current, true) != owner;
        Some((owner, through_arrow))
    }

    fn facts(&mut self, scope: ScopeId) -> &mut FunctionFacts {
        self.functions.entry(scope).or_default()
    }
}

impl<'a> Visit<'a> for FactsVisitor<'_> {
    fn enter_scope(&mut self, _: ScopeFlags, scope_id: &Cell<Option<ScopeId>>) {
        self.scopes
            .push(scope_id.get().expect("semantic analysis binds every scope"));
    }

    fn leave_scope(&mut self) {
        self.scopes.pop();
    }

    fn visit_this_expression(&mut self, it: &ThisExpression) {
        if let Some((owner, through_arrow)) = self.owner() {
            let facts = self.facts(owner);
            facts.uses_this = true;
            facts.this_captured |= through_arrow;
        }
        walk::walk_this_expression(self, it);
    }

    fn visit_new_target(&mut self, it: &NewTarget) {
        if let Some((owner, _)) = self.owner() {
            self.facts(owner).uses_new_target = true;
        }
        walk::walk_new_target(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        // `arguments` is never a declared symbol unless shadowed, in which case oxc resolves it
        let unresolved = it
            .reference_id
            .get()
            .is_none_or(|id| self.scoping.get_reference(id).symbol_id().is_none());
        if unresolved
            && it.name.as_str() == "arguments"
            && let Some((owner, through_arrow)) = self.owner()
        {
            let facts = self.facts(owner);
            facts.uses_arguments = true;
            facts.arguments_captured |= through_arrow;
        }
        walk::walk_identifier_reference(self, it);
    }
}
