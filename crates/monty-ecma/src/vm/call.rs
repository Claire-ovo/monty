//! Calls and construction: frames for compiled functions, dispatch to
//! natives, bound functions and closure creation.

use std::{fmt::Write, mem, sync::Arc};

use monty_types::ResourceError;

use super::{Frame, MAX_REENTRANCY, Step, Vm};
use crate::{
    builtins::{NativeFn, dispatch},
    error::{RunResult, Throw},
    function::{Function, FunctionKind},
    heap::{FunctionObject, HeapId, JsHeapData, ObjectClass, PropFlags, PropertyKey},
    intern::{AtomId, FunctionId, StaticAtom, WellKnownSymbol},
    realm::ErrorKind,
    string::JsString,
    value::JsValue,
};

/// What a function object is made of, copied out of the heap for a call.
enum Callable {
    Closure {
        function: Arc<Function>,
        cells: Vec<HeapId>,
    },
    Native(NativeFn),
    Bound {
        target: HeapId,
        this: JsValue,
        args: Vec<JsValue>,
    },
}

impl Vm<'_> {
    /// `Call n` from bytecode: a compiled callee gets a frame and the loop
    /// continues into it; a native runs now and its result is pushed.
    pub(super) fn call_from_bytecode(&mut self, callee: JsValue, this: JsValue, args: Vec<JsValue>) -> RunResult<Step> {
        if let Some(Callable::Closure { function, cells }) = self.callable(&callee) {
            let result = self.push_closure_frame(function, &cells, callee, this, args, None);
            result.map(|()| Step::Continue)
        } else {
            let result = self.call(callee, this, args)?;
            self.stack.push(result);
            Ok(Step::Continue)
        }
    }

    /// `New n` from bytecode.
    pub(super) fn construct_from_bytecode(&mut self, callee: JsValue, args: Vec<JsValue>) -> RunResult<Step> {
        match self.callable(&callee) {
            Some(Callable::Closure { function, cells }) if function.kind.is_constructor() => {
                let this = self.construct_this(&callee)?;
                let this_copy = this.clone_with_heap(&self.session.heap);
                self.push_closure_frame(function, &cells, callee, this, args, Some(this_copy))?;
                Ok(Step::Continue)
            }
            _ => {
                let result = self.construct(callee, args)?;
                self.stack.push(result);
                Ok(Step::Continue)
            }
        }
    }

    /// Calls any value as a function, running compiled code to completion.
    /// Consumes `callee`, `this` and `args`.
    pub fn call(&mut self, callee: JsValue, this: JsValue, args: Vec<JsValue>) -> RunResult<JsValue> {
        match self.callable(&callee) {
            None => {
                self.drop_all(this, args);
                let shown = self.inspect_short(&callee)?;
                callee.drop_with(&mut self.session.heap);
                Err(self.type_error(&format!("{shown} is not a function")))
            }
            Some(Callable::Native(native)) => {
                callee.drop_with(&mut self.session.heap);
                self.call_native(native, this, args, false)
            }
            Some(Callable::Bound {
                target,
                this: bound_this,
                args: bound_args,
            }) => {
                callee.drop_with(&mut self.session.heap);
                this.drop_with(&mut self.session.heap);
                let mut all = bound_args;
                all.extend(args);
                self.session.heap.inc_ref(target);
                self.call(JsValue::Ref(target), bound_this, all)
            }
            Some(Callable::Closure { function, cells }) => self.enter_nested(|vm| {
                let depth = vm.frames.len();
                vm.push_closure_frame(function, &cells, callee, this, args, None)?;
                vm.run(depth)
            }),
        }
    }

    /// `new callee(...args)`, consuming both.
    pub fn construct(&mut self, callee: JsValue, args: Vec<JsValue>) -> RunResult<JsValue> {
        match self.callable(&callee) {
            Some(Callable::Closure { function, cells }) if function.kind.is_constructor() => {
                let this = self.construct_this(&callee)?;
                let this_copy = this.clone_with_heap(&self.session.heap);
                self.enter_nested(|vm| {
                    let depth = vm.frames.len();
                    vm.push_closure_frame(function, &cells, callee, this, args, Some(this_copy))?;
                    vm.run(depth)
                })
            }
            Some(Callable::Native(native)) if native.is_constructor() => {
                callee.drop_with(&mut self.session.heap);
                self.call_native(native, JsValue::Undefined, args, true)
            }
            Some(Callable::Bound {
                target,
                args: bound_args,
                this,
            }) => {
                callee.drop_with(&mut self.session.heap);
                this.drop_with(&mut self.session.heap);
                let mut all = bound_args;
                all.extend(args);
                self.session.heap.inc_ref(target);
                self.construct(JsValue::Ref(target), all)
            }
            _ => {
                self.drop_all(JsValue::Undefined, args);
                let shown = self.inspect_short(&callee)?;
                callee.drop_with(&mut self.session.heap);
                Err(self.type_error(&format!("{shown} is not a constructor")))
            }
        }
    }

    /// Runs a nested dispatch loop for a call made from Rust, bounding how
    /// deep natives and compiled code may alternate.
    fn enter_nested<T>(&mut self, body: impl FnOnce(&mut Self) -> RunResult<T>) -> RunResult<T> {
        if self.reentrancy >= MAX_REENTRANCY {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        self.reentrancy += 1;
        let result = body(self);
        self.reentrancy -= 1;
        result
    }

    /// The object a constructor call creates: `callee.prototype`, or
    /// `Object.prototype` when that is not an object.
    fn construct_this(&mut self, callee: &JsValue) -> RunResult<JsValue> {
        let prototype = self.get_atom(callee, StaticAtom::Prototype.id())?;
        let proto = match &prototype {
            JsValue::Ref(id) => *id,
            _ => self.session.realm.object_prototype,
        };
        let this = self.new_object_with_proto(Some(proto));
        prototype.drop_with(&mut self.session.heap);
        Ok(this)
    }

    /// What `value` is as a function, with owned copies of its parts.
    fn callable(&self, value: &JsValue) -> Option<Callable> {
        let JsValue::Ref(id) = value else {
            return None;
        };
        let object = self.object(*id)?;
        let ObjectClass::Function(function) = &object.class else {
            return None;
        };
        Some(match function {
            FunctionObject::Closure { func, cells, .. } => Callable::Closure {
                function: Arc::clone(self.session.interns.function(*func)),
                cells: cells.to_vec(),
            },
            FunctionObject::Native(native) => Callable::Native(*native),
            FunctionObject::Bound { target, this, args } => Callable::Bound {
                target: *target,
                this: this.clone_with_heap(&self.session.heap),
                args: args.iter().map(|a| a.clone_with_heap(&self.session.heap)).collect(),
            },
        })
    }

    /// Pushes a frame for a closure: the callee's captured cells, then the
    /// frame's own setup.
    fn push_closure_frame(
        &mut self,
        function: Arc<Function>,
        cells: &[HeapId],
        callee: JsValue,
        this: JsValue,
        args: Vec<JsValue>,
        construct_this: Option<JsValue>,
    ) -> RunResult<()> {
        let result = self.push_frame(function, Some((&callee, cells)), this, args, construct_this);
        callee.drop_with(&mut self.session.heap);
        result
    }

    /// Sets up a frame: locals start as `undefined`, parameters take their
    /// arguments (extras into the rest array), implicit slots are filled and
    /// captured bindings wrapped in cells.
    pub(super) fn push_frame(
        &mut self,
        function: Arc<Function>,
        closure: Option<(&JsValue, &[HeapId])>,
        this: JsValue,
        args: Vec<JsValue>,
        construct_this: Option<JsValue>,
    ) -> RunResult<()> {
        if let Err(err) = self.tracker().check_recursion_depth(self.frames.len() + 1) {
            self.drop_all(this, args);
            if let Some(this) = construct_this {
                this.drop_with(&mut self.session.heap);
            }
            return Err(match err {
                ResourceError::Recursion { .. } => self.range_error("Maximum call stack size exceeded"),
                other => Throw::Resource(other),
            });
        }
        let base = self.stack.len();
        let locals = usize::from(function.locals);
        self.tracker()
            .check_allocation((locals + usize::from(function.code.max_stack)) * 16)?;
        self.stack.resize_with(base + locals, || JsValue::Undefined);
        // parameters
        let formals = usize::from(function.formal_count);
        let fixed = if function.has_rest { formals - 1 } else { formals };
        let mut args = args.into_iter();
        for slot in 0..fixed {
            if let Some(arg) = args.next() {
                self.stack[base + slot] = arg;
            }
        }
        let rest: Vec<JsValue> = args.collect();
        if function.has_rest {
            let array = self.new_array(rest)?;
            self.stack[base + fixed] = array;
        } else if let Some(slot) = function.arguments_slot {
            // `arguments` sees every argument; fixed ones are copied, not aliased (strict mode)
            let mut all: Vec<JsValue> = (0..fixed)
                .map(|i| self.stack[base + i].clone_with_heap(&self.session.heap))
                .collect();
            all.extend(rest);
            let arguments = self.new_arguments(all)?;
            self.stack[base + usize::from(slot)] = arguments;
        } else {
            for value in rest {
                value.drop_with(&mut self.session.heap);
            }
        }
        // implicit bindings
        match function.this_slot {
            Some(slot) => self.stack[base + usize::from(slot)] = this,
            None => this.drop_with(&mut self.session.heap),
        }
        if let (Some(slot), Some((callee, _))) = (function.self_slot, closure) {
            self.stack[base + usize::from(slot)] = callee.clone_with_heap(&self.session.heap);
        }
        if let Some((_, cells)) = closure {
            for (cell, slot) in cells.iter().zip(&function.free_slots) {
                self.session.heap.inc_ref(*cell);
                self.stack[base + usize::from(*slot)] = JsValue::Ref(*cell);
            }
        }
        for entry in &function.entry_cells {
            let index = base + usize::from(entry.slot);
            let seed = mem::replace(&mut self.stack[index], JsValue::Undefined);
            let cell = self.session.heap.allocate(JsHeapData::Cell(seed));
            self.stack[index] = JsValue::Ref(cell);
        }
        self.frames.push(Frame {
            function,
            ip: 0,
            op_start: 0,
            base,
            operands: base + locals,
            construct_this,
        });
        Ok(())
    }

    /// Creates a closure of `id`, copying the current frame's cells it captures.
    pub(super) fn make_closure(&mut self, id: FunctionId, home_object: Option<HeapId>) -> RunResult<JsValue> {
        let function = Arc::clone(self.session.interns.function(id));
        let base = self.frames.last().expect("frame").base;
        let mut cells = Vec::with_capacity(function.captures.len());
        for slot in &function.captures {
            let JsValue::Ref(cell) = &self.stack[base + usize::from(*slot)] else {
                return Err(self.internal_error("captured slot holds no cell"));
            };
            self.session.heap.inc_ref(*cell);
            cells.push(*cell);
        }
        if let Some(home) = home_object {
            self.session.heap.inc_ref(home);
        }
        let callable = FunctionObject::Closure {
            func: id,
            cells: cells.into_boxed_slice(),
            home_object,
        };
        let object = self.new_function_object(callable, function.name, function.param_count);
        if function.kind == FunctionKind::Normal {
            // a constructible function carries a fresh `prototype` whose `constructor` is the function
            let prototype = self.new_object();
            let JsValue::Ref(prototype_id) = &prototype else {
                unreachable!()
            };
            self.session.heap.inc_ref(object);
            self.define_with_flags(
                *prototype_id,
                PropertyKey::Atom(StaticAtom::Constructor.id()),
                JsValue::Ref(object),
                PropFlags::HIDDEN,
            );
            self.define_with_flags(
                object,
                PropertyKey::Atom(StaticAtom::Prototype.id()),
                prototype,
                PropFlags::WRITABLE,
            );
        }
        Ok(JsValue::Ref(object))
    }

    /// Allocates a function object with its `name` and `length`.
    pub fn new_function_object(&mut self, callable: FunctionObject, name: AtomId, length: u16) -> HeapId {
        let proto = self.session.realm.function_prototype;
        let id = self.new_class_object(proto, ObjectClass::Function(callable));
        self.define_with_flags(
            id,
            PropertyKey::Atom(StaticAtom::Length.id()),
            JsValue::Number(f64::from(length)),
            PropFlags::CONFIGURABLE,
        );
        self.define_with_flags(
            id,
            PropertyKey::Atom(StaticAtom::Name.id()),
            JsValue::Atom(name),
            PropFlags::CONFIGURABLE,
        );
        id
    }

    /// The `arguments` object of a call.
    fn new_arguments(&mut self, items: Vec<JsValue>) -> RunResult<JsValue> {
        self.tracker().check_allocation(items.len() * 16)?;
        let proto = self.session.realm.object_prototype;
        let id = self.new_class_object(proto, ObjectClass::Arguments);
        self.object_mut(id).expect("arguments").elements = items;
        let values = self.session.realm.array_values;
        self.session.heap.inc_ref(values);
        self.define_with_flags(
            id,
            PropertyKey::Symbol(WellKnownSymbol::Iterator.id()),
            JsValue::Ref(values),
            PropFlags::HIDDEN,
        );
        Ok(JsValue::Ref(id))
    }

    /// Runs a native, releasing its arguments afterwards.
    fn call_native(
        &mut self,
        native: NativeFn,
        this: JsValue,
        args: Vec<JsValue>,
        construct: bool,
    ) -> RunResult<JsValue> {
        let result = self.enter_nested(|vm| dispatch(vm, native, &this, &args, construct));
        self.drop_all(this, args);
        result
    }

    /// Releases a receiver and arguments that will not be used.
    pub fn drop_all(&mut self, this: JsValue, args: Vec<JsValue>) {
        this.drop_with(&mut self.session.heap);
        for arg in args {
            arg.drop_with(&mut self.session.heap);
        }
    }

    /// Allocates an error object of `kind` with `message`, capturing the
    /// current frames as its `stack`.
    pub fn new_error(&mut self, kind: ErrorKind, message: &str) -> JsValue {
        let proto = self.session.realm.error_prototype(kind);
        self.new_error_with_proto(proto, kind.name(), message)
    }

    /// Allocates an error with an explicit prototype (subclass construction).
    pub fn new_error_with_proto(&mut self, proto: HeapId, name: &str, message: &str) -> JsValue {
        let id = self.new_class_object(proto, ObjectClass::Error);
        if let Ok(message_value) = self.alloc_string(JsString::from(message)) {
            self.define_with_flags(
                id,
                PropertyKey::Atom(StaticAtom::Message.id()),
                message_value,
                PropFlags::HIDDEN,
            );
        }
        let stack = self.capture_stack(name, message);
        if let Ok(stack_value) = self.alloc_string(JsString::from(stack.as_str())) {
            self.define_with_flags(
                id,
                PropertyKey::Atom(StaticAtom::Stack.id()),
                stack_value,
                PropFlags::HIDDEN,
            );
        }
        JsValue::Ref(id)
    }

    /// V8-style `stack` text: the header line then one `at` line per frame.
    fn capture_stack(&self, name: &str, message: &str) -> String {
        let mut text = if message.is_empty() {
            name.to_owned()
        } else {
            format!("{name}: {message}")
        };
        for frame in self.frames.iter().rev().take(10) {
            let function = &frame.function;
            let (start, _) = function
                .code
                .location(frame.op_start)
                .unwrap_or((function.span.start, 0));
            let (line, column) = self
                .session
                .interns
                .source(function.span.feed as usize)
                .map_or((1, 1), |source| line_column(source, start));
            let fn_name = self.session.interns.atom(function.name).to_string_lossy();
            let location = format!("{}:{line}:{column}", self.session.script_name);
            let _ = if fn_name.is_empty() {
                write!(text, "\n    at {location}")
            } else {
                write!(text, "\n    at {fn_name} ({location})")
            };
        }
        text
    }
}

/// 1-based line and column of a byte offset.
fn line_column(source: &str, offset: u32) -> (usize, usize) {
    let offset = (offset as usize).min(source.len());
    let line_start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line = source[..line_start].matches('\n').count() + 1;
    (line, source[line_start..offset].chars().count() + 1)
}
