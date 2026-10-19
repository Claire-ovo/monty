//! Scope facts the compiler needs beyond what `oxc_semantic` binds.
//!
//! oxc resolves every identifier to a symbol and every symbol to a scope; this
//! pass derives what the bytecode shape depends on: which bindings are
//! captured by an inner function (so they live in heap cells) and which
//! functions need their `this`, `arguments` or `new.target` materialized or
//! captured by arrows inside them.

use std::cell::Cell;

use ahash::{AHashMap, AHashSet};
use oxc_ast::ast::{AwaitExpression, IdentifierReference, NewTarget, Program, PropertyDefinition, ThisExpression};
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
    /// The module body itself awaits, so it compiles as an async function.
    pub top_level_await: bool,
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
    /// An arrow inside the body reads `new.target`, so the slot must be a cell.
    pub new_target_captured: bool,
}

impl Analysis {
    /// Derives the facts for an annotated program.
    pub fn new(program: &Program<'_>, scoping: Scoping) -> Self {
        let mut visitor = FactsVisitor {
            scoping: &scoping,
            functions: AHashMap::new(),
            scopes: Vec::new(),
            field_class: Vec::new(),
            captured: captured_symbols(&scoping),
            top_level_await: false,
        };
        visitor.visit_program(program);
        let functions = visitor.functions;
        let captured = visitor.captured;
        let top_level_await = visitor.top_level_await;
        Self {
            scoping,
            captured,
            functions,
            top_level_await,
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

    /// The nearest enclosing scope (`scope` included) that is a function, a
    /// class static block or the module.
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

/// Walks up from `scope` to the nearest function, class static block or
/// module scope. With `arrows` false, arrow functions are skipped: they have
/// no `this` of their own.
fn function_scope(scoping: &Scoping, scope: ScopeId, arrows: bool) -> ScopeId {
    let mut current = scope;
    loop {
        let flags = scoping.scope_flags(current);
        let is_boundary =
            flags.is_top() || flags.is_class_static_block() || (flags.is_function() && (arrows || !flags.is_arrow()));
        if is_boundary {
            return current;
        }
        current = scoping.scope_parent_id(current).expect("a non-root scope has a parent");
    }
}

/// Records `this` / `arguments` / `new.target` use against the owning
/// function, and the extra captures class field initializers need.
struct FactsVisitor<'s> {
    scoping: &'s Scoping,
    functions: AHashMap<ScopeId, FunctionFacts>,
    /// Scopes entered so far, innermost last.
    scopes: Vec<ScopeId>,
    /// The class whose field initializer is being visited, innermost last.
    /// Initializers compile to a synthetic method keyed by the class scope,
    /// so `this` inside them belongs to that key and outer bindings they
    /// read are captured.
    field_class: Vec<ScopeId>,
    captured: AHashSet<SymbolId>,
    top_level_await: bool,
}

impl FactsVisitor<'_> {
    /// The non-arrow function owning the current position, and whether an
    /// arrow sits between it and the position.
    fn owner(&self) -> Option<(ScopeId, bool)> {
        let current = *self.scopes.last()?;
        let mut owner = function_scope(self.scoping, current, false);
        let innermost = function_scope(self.scoping, current, true);
        if let Some(class) = self.field_class.last()
            && !self.inside(owner, *class)
        {
            // a field initializer: `this` is the instance (or the class), reached
            // through an arrow only when that arrow sits inside the initializer
            let through_arrow = self.scoping.scope_flags(innermost).is_arrow() && self.inside(innermost, *class);
            owner = *class;
            return Some((owner, through_arrow));
        }
        Some((owner, innermost != owner))
    }

    /// Whether `scope` is `ancestor` or nested inside it.
    fn inside(&self, scope: ScopeId, ancestor: ScopeId) -> bool {
        scope == ancestor || self.scoping.scope_is_descendant_of(scope, ancestor)
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

    fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
        if let Some(current) = self.scopes.last().copied()
            && self
                .scoping
                .scope_flags(function_scope(self.scoping, current, true))
                .is_top()
        {
            self.top_level_await = true;
        }
        walk::walk_await_expression(self, it);
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
        if let Some((owner, through_arrow)) = self.owner() {
            let facts = self.facts(owner);
            facts.uses_new_target = true;
            facts.new_target_captured |= through_arrow;
        }
        walk::walk_new_target(self, it);
    }

    fn visit_property_definition(&mut self, it: &PropertyDefinition<'a>) {
        // the class scope is the innermost scope entered when the body is visited
        if let Some(class) = self.scopes.last().copied() {
            self.field_class.push(class);
            walk::walk_property_definition(self, it);
            self.field_class.pop();
        }
    }

    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        let symbol = it
            .reference_id
            .get()
            .and_then(|id| self.scoping.get_reference(id).symbol_id());
        // a field initializer runs as its own function: outer bindings it reads live in cells
        if let (Some(symbol), Some(class)) = (symbol, self.field_class.last().copied()) {
            let home = function_scope(self.scoping, self.scoping.symbol_scope_id(symbol), true);
            if !self.inside(home, class) {
                self.captured.insert(symbol);
            }
        }
        let unresolved = symbol.is_none();
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
