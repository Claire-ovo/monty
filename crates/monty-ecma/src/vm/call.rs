//! Calls and construction: frames for compiled functions, dispatch to
//! natives, bound functions and closure creation.

use std::{fmt::Write, mem, sync::Arc};

use monty_types::ResourceError;

use super::{Completion, Construct, Frame, MAX_REENTRANCY, Step, Vm};
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

/// How a frame is entered: its receiver, `new.target` and what `new` does
/// with the return value.
pub(super) struct Invocation {
    pub this: JsValue,
    pub new_target: JsValue,
    pub construct: Construct,
}

impl Invocation {
    /// An ordinary call with `this`.
    pub fn call(this: JsValue) -> Self {
        Self {
            this,
            new_target: JsValue::Undefined,
            construct: Construct::Call,
        }
    }
}

/// What a function object is made of, copied out of the heap for a call.
pub(super) enum Callable {
    Closure {
        function: Arc<Function>,
        cells: Vec<HeapId>,
    },
    Native(NativeFn),
    /// A host function; calling it suspends the feed.
    External(AtomId),
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
        match self.callable(&callee) {
            Some(Callable::Closure { function, .. }) if function.kind.is_class_constructor() => {
                self.drop_all(this, args);
                callee.drop_with(&mut self.session.heap);
                Err(self.class_without_new(&function))
            }
            Some(Callable::Closure { function, cells }) => {
                let is_async = function.kind.is_async();
                self.push_closure_frame(function, &cells, callee, args, Invocation::call(this))?;
                if is_async {
                    self.start_coroutine();
                }
                Ok(Step::Continue)
            }
            Some(Callable::External(name)) => {
                this.drop_with(&mut self.session.heap);
                callee.drop_with(&mut self.session.heap);
                self.external_call(name, args)
            }
            None => {
                self.drop_all(this, args);
                callee.drop_with(&mut self.session.heap);
                let text = self.callee_text();
                Err(self.type_error(&format!("{text} is not a function")))
            }
            Some(_) => {
                let result = self.call(callee, this, args)?;
                self.stack.push(result);
                Ok(Step::Continue)
            }
        }
    }

    /// `New n` from bytecode.
    pub(super) fn construct_from_bytecode(&mut self, callee: JsValue, args: Vec<JsValue>) -> RunResult<Step> {
        match self.callable(&callee) {
            Some(Callable::Closure { function, cells }) if function.kind.is_constructor() => {
                let new_target = callee.clone_with_heap(&self.session.heap);
                self.push_construct_frame(function, &cells, callee, args, new_target)?;
                Ok(Step::Continue)
            }
            Some(Callable::Native(native)) if native.is_constructor() => {
                let result = self.construct(callee, args)?;
                self.stack.push(result);
                Ok(Step::Continue)
            }
            Some(Callable::Bound { .. }) => {
                let result = self.construct(callee, args)?;
                self.stack.push(result);
                Ok(Step::Continue)
            }
            _ => {
                self.drop_all(JsValue::Undefined, args);
                callee.drop_with(&mut self.session.heap);
                let text = self.callee_text();
                Err(self.type_error(&format!("{text} is not a constructor")))
            }
        }
    }

    /// Calls any value as a function, running compiled code to completion.
    /// Consumes `callee`, `this` and `args`.
    pub fn call(&mut self, callee: JsValue, this: JsValue, args: Vec<JsValue>) -> RunResult<JsValue> {
        match self.callable(&callee) {
            None => {
                self.drop_all(this, args);
                let shown = self.inspect_callee(&callee)?;
                callee.drop_with(&mut self.session.heap);
                Err(self.type_error(&format!("{shown} is not a function")))
            }
            Some(Callable::Native(native)) => {
                callee.drop_with(&mut self.session.heap);
                self.call_native(native, this, args, false)
            }
            Some(Callable::External(name)) => {
                self.drop_all(this, args);
                callee.drop_with(&mut self.session.heap);
                let name = self.session.interns.atom(name).to_string_lossy();
                Err(self.type_error(&format!(
                    "Host function '{name}' cannot be called from a callback run by a builtin"
                )))
            }
            Some(Callable::Bound {
                target,
                this: bound_this,
                args: bound_args,
            }) => {
                // the callee may be the target's only owner: take a reference before releasing it
                self.session.heap.inc_ref(target);
                callee.drop_with(&mut self.session.heap);
                this.drop_with(&mut self.session.heap);
                let mut all = bound_args;
                all.extend(args);
                self.call(JsValue::Ref(target), bound_this, all)
            }
            Some(Callable::Closure { function, .. }) if function.kind.is_class_constructor() => {
                self.drop_all(this, args);
                callee.drop_with(&mut self.session.heap);
                Err(self.class_without_new(&function))
            }
            Some(Callable::Closure { function, cells }) => self.enter_nested(|vm| {
                let depth = vm.frames.len();
                let is_async = function.kind.is_async();
                vm.push_closure_frame(function, &cells, callee, args, Invocation::call(this))?;
                if is_async {
                    vm.start_coroutine();
                }
                vm.run_to_return(depth)
            }),
        }
    }

    /// `new callee(...args)`, consuming both.
    pub fn construct(&mut self, callee: JsValue, args: Vec<JsValue>) -> RunResult<JsValue> {
        let new_target = callee.clone_with_heap(&self.session.heap);
        self.construct_with(callee, args, new_target)
    }

    /// Constructs `callee` for `new_target` (the class `new` named, which
    /// decides the instance's prototype through a `super()` chain).
    pub fn construct_with(&mut self, callee: JsValue, args: Vec<JsValue>, new_target: JsValue) -> RunResult<JsValue> {
        match self.callable(&callee) {
            Some(Callable::Closure { function, cells }) if function.kind.is_constructor() => self.enter_nested(|vm| {
                let depth = vm.frames.len();
                vm.push_construct_frame(function, &cells, callee, args, new_target)?;
                vm.run_to_return(depth)
            }),
            Some(Callable::Native(native)) if native.is_constructor() => {
                let subclassed = new_target != callee;
                callee.drop_with(&mut self.session.heap);
                let result = self.call_native(native, JsValue::Undefined, args, true);
                if !subclassed {
                    new_target.drop_with(&mut self.session.heap);
                    return result;
                }
                // `class X extends Error`: the native built the instance; give it the subclass's prototype
                let result = result?;
                let prototype = self.get_atom(&new_target, StaticAtom::Prototype.id());
                new_target.drop_with(&mut self.session.heap);
                let prototype = prototype?;
                if let (JsValue::Ref(instance), JsValue::Ref(proto)) = (&result, &prototype) {
                    self.set_prototype(*instance, Some(*proto));
                }
                prototype.drop_with(&mut self.session.heap);
                Ok(result)
            }
            Some(Callable::Bound {
                target,
                args: bound_args,
                this,
            }) => {
                self.session.heap.inc_ref(target);
                callee.drop_with(&mut self.session.heap);
                this.drop_with(&mut self.session.heap);
                let mut all = bound_args;
                all.extend(args);
                // a bound function's new.target is the target when it was the constructor
                let new_target = if let JsValue::Ref(id) = &new_target
                    && self
                        .object(*id)
                        .is_some_and(|o| matches!(o.class, ObjectClass::Function(FunctionObject::Bound { .. })))
                {
                    new_target.drop_with(&mut self.session.heap);
                    self.session.heap.inc_ref(target);
                    JsValue::Ref(target)
                } else {
                    new_target
                };
                self.construct_with(JsValue::Ref(target), all, new_target)
            }
            _ => {
                self.drop_all(JsValue::Undefined, args);
                new_target.drop_with(&mut self.session.heap);
                let shown = self.inspect_callee(&callee)?;
                callee.drop_with(&mut self.session.heap);
                Err(self.type_error(&format!("{shown} is not a constructor")))
            }
        }
    }

    /// The `TypeError` for calling a class constructor without `new`.
    fn class_without_new(&mut self, function: &Function) -> Throw {
        let name = self.session.interns.atom(function.name).to_string_lossy();
        self.type_error(&format!("Class constructor {name} cannot be invoked without 'new'"))
    }

    /// Pushes the frame of a `new` call: a base constructor gets its instance
    /// up front (and runs its field initializer), a derived one waits for
    /// `super()`.
    fn push_construct_frame(
        &mut self,
        function: Arc<Function>,
        cells: &[HeapId],
        callee: JsValue,
        args: Vec<JsValue>,
        new_target: JsValue,
    ) -> RunResult<()> {
        if function.kind.is_derived_constructor() {
            return self.push_closure_frame(
                function,
                cells,
                callee,
                args,
                Invocation {
                    this: JsValue::Hole,
                    new_target,
                    construct: Construct::Derived,
                },
            );
        }
        let this = match self.construct_this(&new_target) {
            Ok(this) => this,
            Err(err) => {
                self.drop_all(callee, args);
                new_target.drop_with(&mut self.session.heap);
                return Err(err);
            }
        };
        if let JsValue::Ref(class) = &callee
            && let Err(err) = self.run_field_initializer(*class, &this)
        {
            self.drop_all(callee, args);
            this.drop_with(&mut self.session.heap);
            new_target.drop_with(&mut self.session.heap);
            return Err(err);
        }
        let invocation = Invocation {
            construct: Construct::Base(this.clone_with_heap(&self.session.heap)),
            this,
            new_target,
        };
        self.push_closure_frame(function, cells, callee, args, invocation)
    }

    /// Runs a class's instance field initializer on a new instance.
    fn run_field_initializer(&mut self, class: HeapId, this: &JsValue) -> RunResult<()> {
        let initializer = match self.object(class).map(|o| &o.class) {
            Some(ObjectClass::Function(FunctionObject::Closure {
                fields: Some(fields), ..
            })) => *fields,
            _ => return Ok(()),
        };
        self.session.heap.inc_ref(initializer);
        let result = self.call(
            JsValue::Ref(initializer),
            this.clone_with_heap(&self.session.heap),
            Vec::new(),
        )?;
        result.drop_with(&mut self.session.heap);
        Ok(())
    }

    /// `super(...args)`: constructs the parent class for the running
    /// constructor's `new.target`, binds the result as `this` and runs this
    /// class's field initializer.
    pub(super) fn super_call(&mut self, args: Vec<JsValue>) -> RunResult<JsValue> {
        let frame = self.frames.last().expect("frame");
        let Some(class) = frame.callee else {
            self.drop_all(JsValue::Undefined, args);
            return Err(self.internal_error("super() outside a class constructor"));
        };
        let new_target = frame.new_target.clone_with_heap(&self.session.heap);
        let parent = self.get_prototype(class);
        let parent_callable = parent.is_some_and(|parent| {
            self.object(parent).is_some_and(|o| match &o.class {
                ObjectClass::Function(FunctionObject::Closure { func, .. }) => {
                    self.session.interns.function(*func).kind.is_constructor()
                }
                ObjectClass::Function(FunctionObject::Native(native)) => native.is_constructor(),
                ObjectClass::Function(FunctionObject::Bound { .. }) => true,
                _ => false,
            })
        });
        let Some(parent) = parent.filter(|_| parent_callable) else {
            self.drop_all(new_target, args);
            let shown = match parent {
                Some(parent) => self.inspect_short(&JsValue::Ref(parent))?,
                None => "null".to_owned(),
            };
            let name = self
                .session
                .interns
                .atom(self.frames.last().expect("frame").function.name)
                .to_string_lossy();
            let class_name = if name.is_empty() {
                "anonymous class".to_owned()
            } else {
                name
            };
            return Err(self.type_error(&format!(
                "Super constructor {shown} of {class_name} is not a constructor"
            )));
        };
        self.session.heap.inc_ref(parent);
        let this = self.construct_with(JsValue::Ref(parent), args, new_target)?;
        let result = this.clone_with_heap(&self.session.heap);
        self.set_derived_this(this)?;
        self.run_field_initializer(class, &result)?;
        Ok(result)
    }

    /// `super.x`: the prototype of the running method's home object.
    pub(super) fn super_base(&mut self) -> RunResult<JsValue> {
        let frame = self.frames.last().expect("frame");
        let home = frame
            .callee
            .and_then(|callee| match self.object(callee).map(|o| &o.class) {
                Some(ObjectClass::Function(FunctionObject::Closure { home_object, .. })) => *home_object,
                _ => None,
            });
        let Some(home) = home else {
            return Err(self.syntax_error("'super' keyword unexpected here"));
        };
        Ok(match self.get_prototype(home) {
            Some(proto) => {
                self.session.heap.inc_ref(proto);
                JsValue::Ref(proto)
            }
            None => JsValue::Null,
        })
    }

    /// `MakeClass`: links a constructor to its parent and creates its
    /// prototype object. `parent` is the hole for a class without `extends`.
    pub(super) fn make_class(&mut self, constructor: JsValue, parent: JsValue) -> RunResult<JsValue> {
        let JsValue::Ref(class) = &constructor else {
            parent.drop_with(&mut self.session.heap);
            return Err(self.internal_error("MakeClass without a constructor"));
        };
        let class = *class;
        let (proto_parent, constructor_parent) = match &parent {
            JsValue::Hole => (
                Some(self.session.realm.object_prototype),
                self.session.realm.function_prototype,
            ),
            JsValue::Null => (None, self.session.realm.function_prototype),
            JsValue::Ref(id) if self.is_constructor_value(&parent) => {
                let prototype = self.get_atom(&parent, StaticAtom::Prototype.id())?;
                let proto_parent = match prototype {
                    JsValue::Ref(proto) => Some(proto),
                    JsValue::Null => None,
                    other => {
                        let shown = self.inspect_short(&other)?;
                        other.drop_with(&mut self.session.heap);
                        parent.drop_with(&mut self.session.heap);
                        return Err(self.type_error(&format!(
                            "Class extends value does not have valid prototype property {shown}"
                        )));
                    }
                };
                (proto_parent, *id)
            }
            other => {
                let shown = self.inspect(other)?;
                parent.drop_with(&mut self.session.heap);
                return Err(self.type_error(&format!("Class extends value {shown} is not a constructor or null")));
            }
        };
        let prototype = self.new_object_with_proto(proto_parent);
        let JsValue::Ref(prototype_id) = &prototype else {
            unreachable!()
        };
        if let Some(proto_parent) = proto_parent {
            // `get_atom` lent us a reference to the parent's prototype
            self.session.heap.dec_ref(proto_parent);
        }
        self.session.heap.inc_ref(class);
        self.define_with_flags(
            *prototype_id,
            PropertyKey::Atom(StaticAtom::Constructor.id()),
            JsValue::Ref(class),
            PropFlags::HIDDEN,
        );
        self.define_with_flags(
            class,
            PropertyKey::Atom(StaticAtom::Prototype.id()),
            prototype,
            PropFlags::NONE,
        );
        self.set_prototype(class, Some(constructor_parent));
        parent.drop_with(&mut self.session.heap);
        Ok(constructor)
    }

    /// Whether `new` may be applied to the value.
    fn is_constructor_value(&self, value: &JsValue) -> bool {
        let JsValue::Ref(id) = value else {
            return false;
        };
        self.object(*id).is_some_and(|o| match &o.class {
            ObjectClass::Function(FunctionObject::Closure { func, .. }) => {
                self.session.interns.function(*func).kind.is_constructor()
            }
            ObjectClass::Function(FunctionObject::Native(native)) => native.is_constructor(),
            ObjectClass::Function(FunctionObject::Bound { .. }) => true,
            _ => false,
        })
    }

    /// `SetFieldInitializer`: records a class's instance field initializer.
    pub(super) fn set_field_initializer(&mut self, class: HeapId, initializer: JsValue) -> RunResult<()> {
        let JsValue::Ref(id) = initializer else {
            initializer.drop_with(&mut self.session.heap);
            return Err(self.internal_error("field initializer is not a function"));
        };
        if let Some(ObjectClass::Function(FunctionObject::Closure { fields, .. })) =
            self.object_mut(class).map(|o| &mut o.class)
        {
            let old = fields.replace(id);
            if let Some(old) = old {
                self.session.heap.dec_ref(old);
            }
            Ok(())
        } else {
            self.session.heap.dec_ref(id);
            Err(self.internal_error("SetFieldInitializer on a non-class"))
        }
    }

    /// The source text of the callee of the call instruction being executed.
    fn callee_text(&self) -> String {
        let frame = self.frames.last().expect("a frame is running");
        frame.function.code.callee(frame.op_start).map_or_else(
            || "(intermediate value)".to_owned(),
            |atom| self.session.interns.atom(atom).to_string_lossy(),
        )
    }

    /// Runs a nested dispatch loop for a call made from Rust, bounding how
    /// deep natives and compiled code may alternate.
    pub(super) fn enter_nested<T>(&mut self, body: impl FnOnce(&mut Self) -> RunResult<T>) -> RunResult<T> {
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
    pub(super) fn callable(&self, value: &JsValue) -> Option<Callable> {
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
            FunctionObject::External(name) => Callable::External(*name),
            FunctionObject::Bound { target, this, args } => Callable::Bound {
                target: *target,
                this: this.clone_with_heap(&self.session.heap),
                args: args.iter().map(|a| a.clone_with_heap(&self.session.heap)).collect(),
            },
        })
    }

    /// Pushes a frame for a closure: the callee's captured cells, then the
    /// frame's own setup.
    pub(super) fn push_closure_frame(
        &mut self,
        function: Arc<Function>,
        cells: &[HeapId],
        callee: JsValue,
        args: Vec<JsValue>,
        invocation: Invocation,
    ) -> RunResult<()> {
        let result = self.push_frame(function, Some((&callee, cells)), args, invocation);
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
        args: Vec<JsValue>,
        invocation: Invocation,
    ) -> RunResult<()> {
        let Invocation {
            this,
            new_target,
            construct,
        } = invocation;
        if let Err(err) = self.tracker().check_recursion_depth(self.frames.len() + 1) {
            self.drop_all(this, args);
            new_target.drop_with(&mut self.session.heap);
            if let Construct::Base(this) = construct {
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
        if let Some(slot) = function.new_target_slot {
            self.stack[base + usize::from(slot)] = new_target.clone_with_heap(&self.session.heap);
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
        let callee = closure.and_then(|(callee, _)| callee.heap_id());
        if let Some(callee) = callee {
            self.session.heap.inc_ref(callee);
        }
        self.frames.push(Frame {
            function,
            ip: 0,
            op_start: 0,
            base,
            operands: base + locals,
            callee,
            new_target,
            construct,
            generator: None,
            completion: Completion::Deliver,
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
            fields: None,
        };
        let object = self.new_function_object(callable, function.name, function.param_count);
        if function.kind.is_async() {
            let function_proto = self.session.realm.async_function_prototype;
            self.set_prototype(object, Some(function_proto));
        }
        if function.kind.is_generator() {
            // a generator function inherits from %GeneratorFunction.prototype% and
            // carries a fresh `prototype` for its generators, without a `constructor`
            let function_proto = self.session.realm.generator_function_prototype;
            self.set_prototype(object, Some(function_proto));
            let generator_proto = self.session.realm.generator_prototype;
            let prototype = self.new_object_with_proto(Some(generator_proto));
            self.define_with_flags(
                object,
                PropertyKey::Atom(StaticAtom::Prototype.id()),
                prototype,
                PropFlags::WRITABLE,
            );
        }
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
