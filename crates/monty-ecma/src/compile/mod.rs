//! AST to bytecode.
//!
//! One [`Compiler`] handles a feed: the module body is compiled as a function
//! and every nested function is compiled inline, when its expression or
//! declaration is reached, onto a stack of [`FunctionState`]s. Binding
//! resolution is by symbol: oxc's semantic pass has already attached a symbol
//! to every binding identifier and reference, so the compiler maps symbols to
//! frame slots and threads captured symbols through intermediate functions as
//! cells (see [`Compiler::resolve`]).
//!
//! `expr.rs` and `stmt.rs` hold the per-node emission; this file owns the
//! state, scopes, resolution and function assembly.

mod expr;
mod pattern;
mod stmt;

use std::{cell, mem};

use ahash::AHashMap;
use oxc_ast::ast::{
    BindingIdentifier, BindingPattern, BlockStatement, Expression, FormalParameters, Program, Statement,
};
use oxc_semantic::Scoping;
use oxc_span::{GetSpan, Span};
use oxc_syntax::{scope::ScopeId, symbol::SymbolId};

use crate::{
    bytecode::{CodeBuilder, JumpLabel, Opcode},
    error::CompileError,
    function::{EntryCell, Function, FunctionKind, SourceSpan},
    intern::{AtomId, FunctionId, Interns, StaticAtom},
    realm::ScriptScope,
    resolve::Analysis,
    string::JsString,
    value::Constant,
};

/// The feed's compiled code: the module body and the functions it defines,
/// which take the ids `function_base..` once published.
pub(crate) struct CompiledFeed {
    pub module: Function,
    pub functions: Vec<Function>,
}

/// Compiles an analysed program.
///
/// Atoms the snippet names and script-scope bindings it declares are
/// registered as compilation proceeds; a rejected snippet leaves them behind,
/// which costs memory but changes no behaviour (a script binding stays `Hole`).
pub(crate) fn compile(
    program: &Program<'_>,
    analysis: &Analysis,
    feed: u32,
    interns: &mut Interns,
    script: &mut ScriptScope,
) -> Result<CompiledFeed, CompileError> {
    let function_base = u32::try_from(interns.function_count()).map_err(|_| CompileError::TooManyConstants)?;
    let mut compiler = Compiler {
        analysis,
        feed,
        interns,
        script,
        functions: Vec::new(),
        function_base,
        funcs: Vec::new(),
    };
    let module_scope = scope_of(&program.scope_id)?;
    compiler.push_function(FunctionKind::Normal, module_scope, StaticAtom::Empty.id(), program.span);
    compiler.compile_module_statements(program)?;
    let module = compiler.pop_function();
    Ok(CompiledFeed {
        module,
        functions: compiler.functions,
    })
}

/// Compilation state for one feed.
struct Compiler<'c> {
    analysis: &'c Analysis,
    /// The feed's index in the session, recorded in function spans.
    feed: u32,
    interns: &'c mut Interns,
    script: &'c mut ScriptScope,
    /// Finished nested functions, in id order from `function_base`.
    functions: Vec<Function>,
    function_base: u32,
    /// Functions being compiled, the innermost last; never empty while compiling.
    funcs: Vec<FunctionState<'c>>,
}

/// The emission state of one function.
struct FunctionState<'c> {
    builder: CodeBuilder,
    kind: FunctionKind,
    /// The function's own scope (parameters and top-level body).
    scope: ScopeId,
    name: AtomId,
    span: Span,
    /// Where each binding this function owns lives.
    slots: AHashMap<SymbolId, Slot>,
    next_slot: u16,
    /// Bindings of enclosing functions this one closes over, by key.
    captures: Vec<Capture>,
    capture_slots: AHashMap<CaptureKey, u16>,
    this_slot: Option<u16>,
    arguments_slot: Option<u16>,
    new_target_slot: Option<u16>,
    entry_cells: Vec<EntryCell>,
    param_count: u16,
    formal_count: u16,
    has_rest: bool,
    /// Enclosing breakable statements and `finally` blocks, innermost last.
    controls: Vec<Control<'c>>,
    /// A scratch slot for values that must survive an inlined `finally`.
    scratch: Option<u16>,
    /// The slot a named function expression's own name reads.
    self_slot: Option<u16>,
    /// Parameters written as patterns: the slot holding the argument and the
    /// pattern to destructure it with once the scope is declared.
    pattern_params: Vec<(u16, &'c BindingPattern<'c>)>,
    /// Optional chains being compiled, innermost last: each exit jump with
    /// how many operands it leaves to pop.
    chains: Vec<Vec<(JumpLabel, u16)>>,
}

/// A binding's frame slot and whether it holds a cell.
#[derive(Debug, Clone, Copy)]
struct Slot {
    index: u16,
    cell: bool,
}

/// A free variable: the parent frame's slot the cell is copied from, and the
/// slot it occupies in this function's frame.
#[derive(Debug, Clone, Copy)]
struct Capture {
    outer: u16,
    inner: u16,
}

/// What a closure can capture: a declared binding, or the `this` / `arguments`
/// of a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CaptureKey {
    Symbol(SymbolId),
    This(ScopeId),
    Arguments(ScopeId),
}

/// Where a resolved name lives and how to access it.
#[derive(Debug, Clone, Copy)]
enum Place {
    /// A plain frame slot.
    Local(u16),
    /// A frame slot holding a cell.
    Cell(u16),
    /// A top-level `let`/`const`/`class` of the session.
    Script(u32),
    /// A `globalThis` property.
    Global,
}

/// A resolved identifier.
#[derive(Debug, Clone, Copy)]
struct Resolved {
    place: Place,
    name: AtomId,
    /// Reads must throw in the temporal dead zone.
    lexical: bool,
    is_const: bool,
}

/// A statement that `break`/`continue`/`return` may cross or target.
enum Control<'c> {
    Loop {
        label: Option<&'c str>,
        breaks: Vec<JumpLabel>,
        continues: Vec<JumpLabel>,
        /// `for…of` / `for…in` keep their iterator on the operand stack.
        iterator: bool,
    },
    /// The discriminant stays on the operand stack for the statement's duration.
    Switch {
        label: Option<&'c str>,
        breaks: Vec<JumpLabel>,
    },
    Label {
        label: &'c str,
        breaks: Vec<JumpLabel>,
    },
    /// A `try` whose `finally` must run on the way out.
    Finally {
        body: &'c BlockStatement<'c>,
    },
}

impl<'c> Compiler<'c> {
    /// The function being compiled.
    fn func(&mut self) -> &mut FunctionState<'c> {
        self.funcs.last_mut().expect("a function is being compiled")
    }

    fn builder(&mut self) -> &mut CodeBuilder {
        &mut self.func().builder
    }

    /// The module's scope is the first function's.
    fn module_scope(&self) -> ScopeId {
        self.funcs[0].scope
    }

    fn scoping(&self) -> &'c Scoping {
        &self.analysis.scoping
    }

    /// Points subsequent instructions at `span` for tracebacks.
    fn locate(&mut self, span: Span) {
        self.builder().set_location(span.start, span.end);
    }

    // === Functions ===

    /// Starts compiling a function; its slots begin empty.
    fn push_function(&mut self, kind: FunctionKind, scope: ScopeId, name: AtomId, span: Span) {
        self.funcs.push(FunctionState {
            builder: CodeBuilder::new(),
            kind,
            scope,
            name,
            span,
            slots: AHashMap::new(),
            next_slot: 0,
            captures: Vec::new(),
            capture_slots: AHashMap::new(),
            this_slot: None,
            arguments_slot: None,
            new_target_slot: None,
            entry_cells: Vec::new(),
            param_count: 0,
            formal_count: 0,
            has_rest: false,
            controls: Vec::new(),
            scratch: None,
            self_slot: None,
            pattern_params: Vec::new(),
            chains: Vec::new(),
        });
    }

    /// Finishes the innermost function.
    fn pop_function(&mut self) -> Function {
        let state = self.funcs.pop().expect("a function is being compiled");
        let (captures, free_slots) = state.captures.iter().map(|c| (c.outer, c.inner)).unzip();
        Function {
            name: state.name,
            code: state.builder.build(),
            param_count: state.param_count,
            formal_count: state.formal_count,
            has_rest: state.has_rest,
            locals: state.next_slot,
            kind: state.kind,
            this_slot: state.this_slot,
            new_target_slot: state.new_target_slot,
            arguments_slot: state.arguments_slot,
            self_slot: state.self_slot,
            entry_cells: state.entry_cells,
            captures,
            free_slots,
            span: SourceSpan {
                feed: self.feed,
                start: state.span.start,
                end: state.span.end,
            },
        }
    }

    /// Compiles a function body (parameters bound, `this` slot reserved) and
    /// publishes it, returning its id for `MakeClosure`.
    fn compile_function(&mut self, spec: FunctionSpec<'c>) -> Result<FunctionId, CompileError> {
        let FunctionSpec {
            kind,
            scope,
            name,
            span,
            params,
            body,
            self_symbol,
        } = spec;
        self.push_function(kind, scope, name, span);
        self.bind_parameters(params)?;
        if kind != FunctionKind::Arrow {
            self.bind_implicit_slots(scope)?;
        }
        self.declare_scope(scope, true)?;
        if let Some(symbol) = self_symbol {
            let slot = self.declare_symbol(symbol)?;
            self.func().self_slot = Some(slot.index);
        }
        self.emit_parameter_defaults(params)?;
        self.emit_parameter_patterns()?;
        match body {
            FunctionBody::Block(statements) => {
                self.hoist_functions(statements)?;
                self.compile_statements(statements)?;
                if !self.builder().is_dead() {
                    self.builder().emit(Opcode::ReturnUndefined)?;
                }
            }
            FunctionBody::Expression(expression) => {
                self.compile_expression(expression)?;
                self.builder().emit(Opcode::Return)?;
            }
        }
        let function = self.pop_function();
        let id = self.function_base as usize + self.functions.len();
        self.functions.push(function);
        u16::try_from(id)
            .map(FunctionId::from_u16)
            .map_err(|_| CompileError::TooManyConstants)
    }

    /// Assigns the first slots to the formal parameters, in order. A pattern
    /// parameter gets an anonymous slot for its argument and is destructured
    /// by [`emit_parameter_patterns`](Self::emit_parameter_patterns).
    fn bind_parameters(&mut self, params: &'c FormalParameters<'c>) -> Result<(), CompileError> {
        let mut saw_default = false;
        for param in &params.items {
            self.bind_parameter_slot(&param.pattern)?;
            if param.initializer.is_some() {
                saw_default = true;
            } else if !saw_default {
                self.func().param_count += 1;
            }
            self.func().formal_count += 1;
        }
        if let Some(rest) = &params.rest {
            self.bind_parameter_slot(&rest.rest.argument)?;
            self.func().formal_count += 1;
            self.func().has_rest = true;
        }
        Ok(())
    }

    /// The slot one parameter's argument lands in.
    fn bind_parameter_slot(&mut self, pattern: &'c BindingPattern<'c>) -> Result<(), CompileError> {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.declare_symbol(symbol_of(ident)?)?;
            }
            other => {
                let slot = self.alloc_slot()?;
                self.func().pattern_params.push((slot, other));
            }
        }
        Ok(())
    }

    /// Destructures the pattern parameters from their argument slots.
    fn emit_parameter_patterns(&mut self) -> Result<(), CompileError> {
        let params = mem::take(&mut self.func().pattern_params);
        for (slot, pattern) in params {
            self.builder().emit_u16(Opcode::LoadLocal, slot)?;
            self.bind_pattern(pattern, None)?;
        }
        Ok(())
    }

    /// Reserves `this` (always, for non-arrow functions), `arguments` and
    /// `new.target` slots as the body needs them.
    fn bind_implicit_slots(&mut self, scope: ScopeId) -> Result<(), CompileError> {
        let facts = self.analysis.function_facts(scope);
        let this_slot = self.alloc_slot()?;
        self.func().this_slot = Some(this_slot);
        if facts.this_captured {
            self.func().entry_cells.push(EntryCell { slot: this_slot });
            self.func().capture_slots.insert(CaptureKey::This(scope), this_slot);
        }
        if facts.uses_arguments {
            let slot = self.alloc_slot()?;
            self.func().arguments_slot = Some(slot);
            if facts.arguments_captured {
                self.func().entry_cells.push(EntryCell { slot });
                self.func().capture_slots.insert(CaptureKey::Arguments(scope), slot);
            }
        }
        if facts.uses_new_target {
            let slot = self.alloc_slot()?;
            self.func().new_target_slot = Some(slot);
        }
        Ok(())
    }

    /// Emits `param = default` for parameters whose argument is `undefined`.
    fn emit_parameter_defaults(&mut self, params: &'c FormalParameters<'c>) -> Result<(), CompileError> {
        let mut pattern_index = 0;
        for param in &params.items {
            let place = if let BindingPattern::BindingIdentifier(ident) = &param.pattern {
                ParamPlace::Binding(self.resolve_symbol(symbol_of(ident)?)?)
            } else {
                let slot = self.func().pattern_params[pattern_index].0;
                pattern_index += 1;
                ParamPlace::Slot(slot)
            };
            let Some(initializer) = &param.initializer else {
                continue;
            };
            self.locate(param.span);
            match place {
                ParamPlace::Binding(resolved) => self.emit_load(resolved)?,
                ParamPlace::Slot(slot) => self.builder().emit_u16(Opcode::LoadLocal, slot)?,
            }
            let skip = self.builder().emit_jump(Opcode::JumpIfUndefinedKeep)?;
            // not undefined: the value was popped by the fall-through, nothing to do
            let done = self.builder().emit_jump(Opcode::Jump)?;
            self.builder().patch_jump(skip)?;
            self.builder().emit(Opcode::Pop)?;
            let name = match place {
                ParamPlace::Binding(resolved) => Some(resolved.name),
                ParamPlace::Slot(_) => None,
            };
            self.compile_expression_named(initializer, name)?;
            match place {
                ParamPlace::Binding(resolved) => self.emit_store(resolved)?,
                ParamPlace::Slot(slot) => self.builder().emit_u16(Opcode::StoreLocal, slot)?,
            }
            self.builder().patch_jump(done)?;
        }
        Ok(())
    }

    // === Scopes and slots ===

    /// Allocates a slot in the current function.
    fn alloc_slot(&mut self) -> Result<u16, CompileError> {
        let state = self.func();
        let slot = state.next_slot;
        state.next_slot = slot.checked_add(1).ok_or(CompileError::TooManyLocals)?;
        Ok(slot)
    }

    /// The function's scratch slot, allocated on first use.
    fn scratch_slot(&mut self) -> Result<u16, CompileError> {
        if let Some(slot) = self.func().scratch {
            Ok(slot)
        } else {
            let slot = self.alloc_slot()?;
            self.func().scratch = Some(slot);
            Ok(slot)
        }
    }

    /// Gives `symbol` a slot in the current function, a cell when captured.
    fn declare_symbol(&mut self, symbol: SymbolId) -> Result<Slot, CompileError> {
        if let Some(slot) = self.func().slots.get(&symbol) {
            return Ok(*slot);
        }
        let slot = Slot {
            index: self.alloc_slot()?,
            cell: self.analysis.is_captured(symbol),
        };
        self.func().slots.insert(symbol, slot);
        if slot.cell {
            self.func().capture_slots.insert(CaptureKey::Symbol(symbol), slot.index);
        }
        Ok(slot)
    }

    /// Enters a scope: allocates its bindings and initializes them.
    ///
    /// Lexical bindings start as the hole (their temporal dead zone); `var`s
    /// as `undefined`. A parameter already has its slot and its argument, and
    /// a captured one is wrapped into a cell at call time instead. Module
    /// scope bindings live outside the frame and need nothing here.
    fn declare_scope(&mut self, scope: ScopeId, function_entry: bool) -> Result<(), CompileError> {
        if scope == self.module_scope() {
            for symbol in self.scope_symbols(scope) {
                let flags = self.scoping().symbol_flags(symbol);
                let name = self.intern_symbol_name(symbol);
                if flags.is_block_scoped() || flags.is_class() {
                    self.script.declare(name, flags.is_const_variable());
                } else if !flags.is_function() {
                    // a hoisted `var` exists (as `undefined`) before its declaration runs,
                    // keeping the value an earlier feed gave it
                    let operand = atom_operand(name)?;
                    self.builder().emit_u16(Opcode::LoadGlobalOrUndefined, operand)?;
                    self.builder().emit_u16(Opcode::DefineGlobal, operand)?;
                }
            }
            return Ok(());
        }
        for symbol in self.scope_symbols(scope) {
            let already_bound = self.func().slots.contains_key(&symbol);
            let slot = self.declare_symbol(symbol)?;
            let flags = self.scoping().symbol_flags(symbol);
            let lexical = flags.is_block_scoped() || flags.is_class() || flags.is_catch_variable();
            if already_bound && function_entry {
                // a parameter: its argument is in place, only a capture needs a cell
                if slot.cell {
                    self.func().entry_cells.push(EntryCell { slot: slot.index });
                }
            } else if slot.cell {
                if function_entry && !lexical {
                    // `var` and function declarations in a function scope: a cell holding `undefined`
                    self.func().entry_cells.push(EntryCell { slot: slot.index });
                } else {
                    self.builder().emit_u16(Opcode::BuildCell, slot.index)?;
                }
            } else if lexical || !function_entry {
                let op = if lexical {
                    Opcode::LoadHole
                } else {
                    Opcode::LoadUndefined
                };
                self.builder().emit(op)?;
                self.builder().emit_u16(Opcode::StoreLocal, slot.index)?;
            }
        }
        Ok(())
    }

    /// The symbols a scope declares, in source order so slot numbering is stable.
    fn scope_symbols(&self, scope: ScopeId) -> Vec<SymbolId> {
        let scoping = self.scoping();
        let mut symbols: Vec<SymbolId> = scoping.get_bindings(scope).values().copied().collect();
        symbols.sort_by_key(|symbol| scoping.symbol_span(*symbol).start);
        symbols
    }

    /// Creates the closures of a statement list's function declarations
    /// before anything else runs (function hoisting).
    fn hoist_functions(&mut self, statements: &'c [Statement<'c>]) -> Result<(), CompileError> {
        for statement in statements {
            if let Statement::FunctionDeclaration(function) = statement {
                let ident = function
                    .id
                    .as_ref()
                    .ok_or(CompileError::internal("unnamed declaration"))?;
                let name = self.intern_str(ident.name.as_str());
                let id = self.compile_function_node(function, name)?;
                self.locate(function.span);
                self.emit_make_closure(id)?;
                let resolved = self.resolve_symbol(symbol_of(ident)?)?;
                self.emit_initialize(resolved)?;
            }
        }
        Ok(())
    }

    // === Resolution ===

    /// Resolves a symbol from the current function, threading captures
    /// through every function between its declaration and here.
    fn resolve_symbol(&mut self, symbol: SymbolId) -> Result<Resolved, CompileError> {
        let scoping = self.scoping();
        let flags = scoping.symbol_flags(symbol);
        let lexical = flags.is_block_scoped() || flags.is_class();
        let is_const = flags.is_const_variable();
        let name = self.intern_symbol_name(symbol);
        let declaring_scope = scoping.symbol_scope_id(symbol);
        if declaring_scope == self.module_scope() {
            let place = if lexical {
                Place::Script(self.script.declare(name, is_const))
            } else {
                Place::Global
            };
            return Ok(Resolved {
                place,
                name,
                lexical,
                is_const,
            });
        }
        let owner_function = self.analysis.function_scope(declaring_scope);
        let owner_level = self
            .funcs
            .iter()
            .rposition(|state| state.scope == owner_function)
            .ok_or(CompileError::internal(
                "binding declared outside the compiling functions",
            ))?;
        let own_slot = *self.funcs[owner_level]
            .slots
            .get(&symbol)
            .ok_or(CompileError::internal(
                "binding referenced before its scope was declared",
            ))?;
        let place = if owner_level == self.funcs.len() - 1 {
            if own_slot.cell {
                Place::Cell(own_slot.index)
            } else {
                Place::Local(own_slot.index)
            }
        } else {
            Place::Cell(self.thread_capture(CaptureKey::Symbol(symbol), owner_level, own_slot.index)?)
        };
        Ok(Resolved {
            place,
            name,
            lexical,
            is_const,
        })
    }

    /// Resolves a name no symbol binds: a top-level binding of an earlier feed,
    /// else a `globalThis` property.
    fn resolve_unbound(&mut self, name: &str) -> Resolved {
        let name = self.intern_str(name);
        match self.script.lookup(name) {
            Some((index, is_const)) => Resolved {
                place: Place::Script(index),
                name,
                lexical: true,
                is_const,
            },
            None => Resolved {
                place: Place::Global,
                name,
                lexical: false,
                is_const: false,
            },
        }
    }

    /// Makes the cell in `owner_level`'s slot `owner_slot` available in the
    /// current function, adding a capture to every function in between.
    fn thread_capture(&mut self, key: CaptureKey, owner_level: usize, owner_slot: u16) -> Result<u16, CompileError> {
        let mut outer = owner_slot;
        for level in owner_level + 1..self.funcs.len() {
            if let Some(slot) = self.funcs[level].capture_slots.get(&key) {
                outer = *slot;
                continue;
            }
            let state = &mut self.funcs[level];
            let inner = state.next_slot;
            state.next_slot = inner.checked_add(1).ok_or(CompileError::TooManyLocals)?;
            state.captures.push(Capture { outer, inner });
            state.capture_slots.insert(key, inner);
            outer = inner;
        }
        Ok(outer)
    }

    /// Resolves `this` for the current position: the owning non-arrow
    /// function's receiver slot, through captures from inside arrows. `None`
    /// in module code, where `this` is `undefined`.
    fn resolve_this(&mut self) -> Result<Option<Place>, CompileError> {
        self.resolve_implicit(|state| state.this_slot, CaptureKey::This)
    }

    /// Resolves `arguments` like [`resolve_this`](Self::resolve_this); `None`
    /// in module code, where it is an ordinary (undeclared) global.
    fn resolve_arguments(&mut self) -> Result<Option<Place>, CompileError> {
        self.resolve_implicit(|state| state.arguments_slot, CaptureKey::Arguments)
    }

    /// Resolves an implicit per-function binding held in `slot_of` the owning
    /// non-arrow function, captured under `key` from arrows inside it.
    fn resolve_implicit(
        &mut self,
        slot_of: impl Fn(&FunctionState<'c>) -> Option<u16>,
        key: impl Fn(ScopeId) -> CaptureKey,
    ) -> Result<Option<Place>, CompileError> {
        let owner_level = self
            .funcs
            .iter()
            .rposition(|state| state.kind != FunctionKind::Arrow)
            .expect("the module function is never an arrow");
        if owner_level == 0 {
            return Ok(None);
        }
        let owner = &self.funcs[owner_level];
        let slot = slot_of(owner).ok_or(CompileError::internal("implicit binding without a slot"))?;
        let key = key(owner.scope);
        let is_cell = owner.capture_slots.contains_key(&key);
        let place = if owner_level == self.funcs.len() - 1 {
            if is_cell { Place::Cell(slot) } else { Place::Local(slot) }
        } else {
            Place::Cell(self.thread_capture(key, owner_level, slot)?)
        };
        Ok(Some(place))
    }

    // === Emission helpers ===

    fn emit_load(&mut self, resolved: Resolved) -> Result<(), CompileError> {
        let name = atom_operand(resolved.name)?;
        match resolved.place {
            Place::Local(slot) if resolved.lexical => self.builder().emit_u16_u16(Opcode::LoadLocalChecked, slot, name),
            Place::Local(slot) => self.builder().emit_u16(Opcode::LoadLocal, slot),
            Place::Cell(slot) if resolved.lexical => self.builder().emit_u16_u16(Opcode::LoadCellChecked, slot, name),
            Place::Cell(slot) => self.builder().emit_u16(Opcode::LoadCell, slot),
            Place::Script(index) => self
                .builder()
                .emit_u16_u16(Opcode::LoadScript, index_operand(index)?, name),
            Place::Global => self.builder().emit_u16(Opcode::LoadGlobal, name),
        }
    }

    /// Stores the top value into a binding by assignment (`x = v`): a `const`
    /// throws, and a top-level `var` must already exist.
    fn emit_store(&mut self, resolved: Resolved) -> Result<(), CompileError> {
        if resolved.is_const {
            return self
                .builder()
                .emit_u16(Opcode::ThrowConstAssign, atom_operand(resolved.name)?);
        }
        match resolved.place {
            Place::Local(slot) => self.builder().emit_u16(Opcode::StoreLocal, slot),
            Place::Cell(slot) => self.builder().emit_u16(Opcode::StoreCell, slot),
            Place::Script(index) => self.builder().emit_u16(Opcode::StoreScript, index_operand(index)?),
            Place::Global => self
                .builder()
                .emit_u16(Opcode::StoreGlobal, atom_operand(resolved.name)?),
        }
    }

    /// Stores the top value into a binding by declaration (`const x = v`),
    /// which may initialize a `const` and creates a top-level `var`.
    fn emit_initialize(&mut self, resolved: Resolved) -> Result<(), CompileError> {
        match resolved.place {
            Place::Local(slot) => self.builder().emit_u16(Opcode::StoreLocal, slot),
            Place::Cell(slot) => self.builder().emit_u16(Opcode::StoreCell, slot),
            Place::Script(index) => self.builder().emit_u16(Opcode::StoreScript, index_operand(index)?),
            Place::Global => self
                .builder()
                .emit_u16(Opcode::DefineGlobal, atom_operand(resolved.name)?),
        }
    }

    fn emit_make_closure(&mut self, id: FunctionId) -> Result<(), CompileError> {
        let operand = id.as_u16().ok_or(CompileError::TooManyConstants)?;
        self.builder().emit_u16(Opcode::MakeClosure, operand)
    }

    /// Pushes an interned string constant.
    fn emit_atom(&mut self, atom: AtomId) -> Result<(), CompileError> {
        let index = self.builder().add_const(Constant::Atom(atom))?;
        self.builder().emit_u16(Opcode::LoadConst, index)
    }

    fn emit_number(&mut self, value: f64) -> Result<(), CompileError> {
        #[expect(clippy::cast_possible_truncation, reason = "checked to be an exact small integer")]
        if value.fract() == 0.0 && (-128.0..=127.0).contains(&value) && !(value == 0.0 && value.is_sign_negative()) {
            self.builder().emit_i8(Opcode::LoadInt, value as i8)
        } else {
            let index = self.builder().add_const(Constant::Number(value))?;
            self.builder().emit_u16(Opcode::LoadConst, index)
        }
    }

    // === Interning ===

    fn intern_str(&mut self, text: &str) -> AtomId {
        self.interns.intern_str(text)
    }

    fn intern_units(&mut self, units: impl Iterator<Item = u16>) -> AtomId {
        self.interns.intern_untracked(JsString::from_unit_iter(units))
    }

    fn intern_symbol_name(&mut self, symbol: SymbolId) -> AtomId {
        let name = self.scoping().symbol_name(symbol);
        self.interns.intern_str(name)
    }

    /// A span as the `(start, end)` pair errors carry.
    fn span_of(node: &impl GetSpan) -> (u32, u32) {
        let span = node.span();
        (span.start, span.end)
    }
}

/// Everything [`Compiler::compile_function`] needs to know about a function.
#[derive(Clone, Copy)]
struct FunctionSpec<'c> {
    kind: FunctionKind,
    scope: ScopeId,
    name: AtomId,
    span: Span,
    params: &'c FormalParameters<'c>,
    body: FunctionBody<'c>,
    /// The name binding of a named function expression.
    self_symbol: Option<SymbolId>,
}

/// Where a parameter's argument lives while defaults are applied.
#[derive(Clone, Copy)]
enum ParamPlace {
    Binding(Resolved),
    Slot(u16),
}

/// How a function's body is written.
#[derive(Clone, Copy)]
enum FunctionBody<'c> {
    Block(&'c [Statement<'c>]),
    /// An arrow's expression body: its value is returned.
    Expression(&'c Expression<'c>),
}

/// The scope oxc bound to a node.
fn scope_of(cell: &cell::Cell<Option<ScopeId>>) -> Result<ScopeId, CompileError> {
    cell.get()
        .ok_or(CompileError::internal("scope not bound by semantic analysis"))
}

/// The symbol oxc bound to a binding identifier.
fn symbol_of(ident: &BindingIdentifier<'_>) -> Result<SymbolId, CompileError> {
    ident
        .symbol_id
        .get()
        .ok_or(CompileError::internal("binding not bound by semantic analysis"))
}

/// An atom as a 16-bit operand.
fn atom_operand(atom: AtomId) -> Result<u16, CompileError> {
    atom.as_u16().ok_or(CompileError::TooManyConstants)
}

/// A script-scope index as a 16-bit operand.
fn index_operand(index: u32) -> Result<u16, CompileError> {
    u16::try_from(index).map_err(|_| CompileError::TooManyLocals)
}
