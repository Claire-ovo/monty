//! The bytecode interpreter.
//!
//! A [`Vm`] owns a session's heap, interns, realm and script scope for the
//! duration of a feed, plus one interleaved value stack holding every frame's
//! locals and operands. [`Vm::run`] is the dispatch loop; the other files in
//! this module implement the language's abstract operations on top of it:
//! `ops.rs` (conversions, operators), `property.rs` (the object model),
//! `call.rs` (calls, construction, closures), `number.rs` (number formatting)
//! and `inspect.rs` (`console.log` formatting).

pub(crate) mod call;
pub(crate) mod inspect;
pub(crate) mod number;
pub(crate) mod ops;
pub(crate) mod property;

use std::{mem, string::ToString, sync::Arc};

use monty_heap::ContainsHeap;
use monty_types::{MontyException, PrintWriter, ResourceError, ResourceTracker, StackFrame};

use crate::{
    bytecode::{Code, Opcode},
    error::{RunResult, Throw, stack_frame},
    function::Function,
    heap::{Heap, HeapId, JsHeapData, JsObject, PropertyKey},
    intern::{AtomId, FunctionId, Interns},
    realm::{ErrorKind, Realm, ScriptScope},
    string::JsString,
    value::JsValue,
};

/// Instructions between resource checkpoints (time, memory, GC).
const CHECK_INTERVAL: u8 = 255;

/// Nested `run` loops a native may stack (a callback calling a builtin
/// calling a callback…) before the Rust stack is protected by a `RangeError`.
const MAX_REENTRANCY: usize = 200;

/// Everything a session keeps between feeds.
pub(crate) struct Session {
    pub heap: Heap,
    pub interns: Interns,
    pub realm: Realm,
    pub script: ScriptScope,
    pub script_name: String,
    /// Feeds run so far; the next feed's index.
    pub feed_count: u32,
    /// `Math.random`'s generator state.
    pub random_state: u64,
}

/// The interpreter for one feed: the session plus the run-time stacks.
pub(crate) struct Vm<'p> {
    pub session: Session,
    pub print: PrintWriter<'p>,
    /// Locals and operands of every frame, interleaved.
    stack: Vec<JsValue>,
    frames: Vec<Frame>,
    /// The exception a handler is about to receive through `PushException`.
    pending: Option<JsValue>,
    /// Frames an uncaught throw has unwound, innermost first.
    traceback: Vec<StackFrame>,
    ticks: u8,
    /// Depth of nested `run` loops entered from natives.
    reentrancy: usize,
}

/// A function activation on the shared stack.
struct Frame {
    function: Arc<Function>,
    ip: usize,
    /// Offset of the instruction being executed, for locations.
    op_start: usize,
    /// Stack index of local slot 0.
    base: usize,
    /// Stack index where the operand stack starts (`base + locals`).
    operands: usize,
    /// `new` call: the object under construction, returned unless the body
    /// returns an object of its own.
    construct_this: Option<JsValue>,
}

impl<'p> Vm<'p> {
    pub fn new(session: Session, print: PrintWriter<'p>) -> Self {
        Self {
            session,
            print,
            stack: Vec::new(),
            frames: Vec::new(),
            pending: None,
            traceback: Vec::new(),
            ticks: 0,
            reentrancy: 0,
        }
    }

    /// Gives the session back once the feed is over.
    pub fn into_session(mut self) -> Session {
        self.clear_stack(0);
        if let Some(pending) = self.pending.take() {
            pending.drop_with(&mut self.session.heap);
        }
        self.session
    }

    pub fn tracker(&self) -> &ResourceTracker {
        &self.session.heap.tracker
    }

    pub fn realm(&self) -> &Realm {
        &self.session.realm
    }

    /// Runs a module body to completion, returning its completion value.
    pub fn run_module(&mut self, module: Arc<Function>) -> RunResult<JsValue> {
        self.traceback.clear();
        self.push_frame(module, None, JsValue::Undefined, Vec::new(), None)?;
        self.run(0)
    }

    /// The frames an uncaught throw unwound, outermost first.
    pub fn take_traceback(&mut self) -> Vec<StackFrame> {
        let mut frames = mem::take(&mut self.traceback);
        frames.reverse();
        frames
    }

    // === Dispatch ===

    /// Executes frames until the one at depth `stop_depth` returns, yielding
    /// its value. A throw no frame above `stop_depth` catches unwinds those
    /// frames and surfaces as `Err`.
    fn run(&mut self, stop_depth: usize) -> RunResult<JsValue> {
        loop {
            match self.step() {
                Ok(Step::Continue) => {}
                Ok(Step::Return(value)) => {
                    let frame = self.frames.pop().expect("returning from a frame");
                    self.clear_stack(frame.base);
                    let value = match frame.construct_this {
                        Some(this) if !value.is_object() => {
                            value.drop_with(&mut self.session.heap);
                            this
                        }
                        Some(this) => {
                            this.drop_with(&mut self.session.heap);
                            value
                        }
                        None => value,
                    };
                    if self.frames.len() == stop_depth {
                        return Ok(value);
                    }
                    self.stack.push(value);
                }
                Err(Throw::Value(value)) => self.unwind(value, stop_depth)?,
                Err(Throw::Resource(err)) => {
                    // a resource error ends the feed: record where it happened
                    while self.frames.len() > stop_depth {
                        self.record_frame();
                        let frame = self.frames.pop().expect("frame");
                        self.clear_stack(frame.base);
                    }
                    return Err(Throw::Resource(err));
                }
            }
        }
    }

    /// Delivers a thrown value to the nearest handler above `stop_depth`, or
    /// unwinds those frames and propagates it.
    fn unwind(&mut self, value: JsValue, stop_depth: usize) -> RunResult<()> {
        while self.frames.len() > stop_depth {
            let frame = self.frames.last().expect("frame");
            if let Some(entry) = frame.function.code.handler_for(frame.op_start).copied() {
                let operands = frame.operands;
                let frame = self.frames.last_mut().expect("frame");
                frame.ip = entry.handler as usize;
                let depth = operands + usize::from(entry.stack_depth);
                self.clear_stack(depth);
                if let Some(old) = self.pending.replace(value) {
                    old.drop_with(&mut self.session.heap);
                }
                self.traceback.clear();
                return Ok(());
            }
            self.record_frame();
            let frame = self.frames.pop().expect("frame");
            self.clear_stack(frame.base);
        }
        Err(Throw::Value(value))
    }

    /// Appends the top frame's position to the traceback being built.
    fn record_frame(&mut self) {
        let frame = self.frames.last().expect("frame");
        let function = &frame.function;
        let (start, end) = function
            .code
            .location(frame.op_start)
            .unwrap_or((function.span.start, function.span.end));
        let source = self
            .session
            .interns
            .source(function.span.feed as usize)
            .map(ToString::to_string)
            .unwrap_or_default();
        let name = self.session.interns.atom(function.name);
        let frame_name = (!name.is_empty()).then(|| name.to_string_lossy());
        let script_name = self.session.script_name.clone();
        self.traceback
            .push(stack_frame(&script_name, &source, start, end, frame_name));
    }

    /// Executes one instruction of the top frame.
    #[expect(clippy::too_many_lines, reason = "the dispatch loop is one match over every opcode")]
    fn step(&mut self) -> RunResult<Step> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks == CHECK_INTERVAL {
            self.ticks = 0;
            self.checkpoint()?;
        }
        let frame = self.frames.last_mut().expect("a frame is running");
        let code = &frame.function.code;
        frame.op_start = frame.ip;
        let Some(op) = code.bytecode.get(frame.ip).copied().and_then(Opcode::from_repr) else {
            return Err(self.internal_error("invalid opcode"));
        };
        frame.ip += 1;
        match op {
            Opcode::Pop => {
                let value = self.pop();
                value.drop_with(&mut self.session.heap);
            }
            Opcode::Dup => {
                let value = self.peek(0).clone_with_heap(&self.session.heap);
                self.stack.push(value);
            }
            Opcode::Dup2 => {
                let b = self.peek(0).clone_with_heap(&self.session.heap);
                let a = self.peek(1).clone_with_heap(&self.session.heap);
                self.stack.push(a);
                self.stack.push(b);
            }
            Opcode::Swap => {
                let len = self.stack.len();
                self.stack.swap(len - 1, len - 2);
            }
            Opcode::Rot3 => {
                let len = self.stack.len();
                self.stack[len - 3..].rotate_right(1);
            }
            Opcode::Rot4 => {
                let len = self.stack.len();
                self.stack[len - 4..].rotate_right(1);
            }
            Opcode::LoadConst => {
                let index = self.operand_u16();
                let value = self.code().constants[usize::from(index)].to_value();
                self.stack.push(value);
            }
            Opcode::LoadUndefined => self.stack.push(JsValue::Undefined),
            Opcode::LoadNull => self.stack.push(JsValue::Null),
            Opcode::LoadTrue => self.stack.push(JsValue::Bool(true)),
            Opcode::LoadFalse => self.stack.push(JsValue::Bool(false)),
            Opcode::LoadInt => {
                let value = self.operand_i8();
                self.stack.push(JsValue::Number(f64::from(value)));
            }
            Opcode::LoadHole => self.stack.push(JsValue::Hole),
            Opcode::LoadLocal => {
                let slot = self.operand_u16();
                let value = self.local(slot).clone_with_heap(&self.session.heap);
                self.stack.push(value);
            }
            Opcode::LoadLocalChecked => {
                let slot = self.operand_u16();
                let name = AtomId::from_u16(self.operand_u16());
                let value = self.local(slot).clone_with_heap(&self.session.heap);
                if value == JsValue::Hole {
                    return Err(self.tdz_error(name));
                }
                self.stack.push(value);
            }
            Opcode::StoreLocal => {
                let slot = self.operand_u16();
                let value = self.pop();
                let old = mem::replace(self.local_mut(slot), value);
                old.drop_with(&mut self.session.heap);
            }
            Opcode::BuildCell => {
                let slot = self.operand_u16();
                let cell = self.session.heap.allocate(JsHeapData::Cell(JsValue::Hole));
                let old = mem::replace(self.local_mut(slot), JsValue::Ref(cell));
                old.drop_with(&mut self.session.heap);
            }
            Opcode::LoadCell => {
                let slot = self.operand_u16();
                let value = self.cell_value(slot);
                self.stack.push(value);
            }
            Opcode::LoadCellChecked => {
                let slot = self.operand_u16();
                let name = AtomId::from_u16(self.operand_u16());
                let value = self.cell_value(slot);
                if value == JsValue::Hole {
                    return Err(self.tdz_error(name));
                }
                self.stack.push(value);
            }
            Opcode::StoreCell => {
                let slot = self.operand_u16();
                let value = self.pop();
                let JsValue::Ref(cell) = self.local(slot) else {
                    return Err(self.internal_error("StoreCell on a slot without a cell"));
                };
                let cell = *cell;
                let JsHeapData::Cell(contents) = self.session.heap.get_mut(cell) else {
                    return Err(self.internal_error("StoreCell on a non-cell"));
                };
                let old = mem::replace(contents, value);
                old.drop_with(&mut self.session.heap);
            }
            Opcode::CopyCell => {
                let slot = self.operand_u16();
                let value = self.cell_value(slot);
                let cell = self.session.heap.allocate(JsHeapData::Cell(value));
                let old = mem::replace(self.local_mut(slot), JsValue::Ref(cell));
                old.drop_with(&mut self.session.heap);
            }
            Opcode::ThrowConstAssign => {
                let _ = self.operand_u16();
                return Err(self.type_error("Assignment to constant variable."));
            }
            Opcode::LoadGlobal => {
                let name = AtomId::from_u16(self.operand_u16());
                let global = self.session.realm.global;
                let Some(value) = self.get_own_or_inherited(global, PropertyKey::Atom(name))? else {
                    return Err(self.reference_error(name));
                };
                self.stack.push(value);
            }
            Opcode::LoadGlobalOrUndefined => {
                let name = AtomId::from_u16(self.operand_u16());
                let global = self.session.realm.global;
                let value = self
                    .get_own_or_inherited(global, PropertyKey::Atom(name))?
                    .unwrap_or(JsValue::Undefined);
                self.stack.push(value);
            }
            Opcode::StoreGlobal => {
                let name = AtomId::from_u16(self.operand_u16());
                let value = self.pop();
                let global = self.session.realm.global;
                if !self.has_property(global, PropertyKey::Atom(name)) {
                    value.drop_with(&mut self.session.heap);
                    return Err(self.reference_error(name));
                }
                self.set(&JsValue::Ref(global), PropertyKey::Atom(name), value)?;
            }
            Opcode::DefineGlobal => {
                let name = AtomId::from_u16(self.operand_u16());
                let value = self.pop();
                let global = self.session.realm.global;
                self.set(&JsValue::Ref(global), PropertyKey::Atom(name), value)?;
            }
            Opcode::LoadScript => {
                let index = usize::from(self.operand_u16());
                let name = AtomId::from_u16(self.operand_u16());
                let value = self.session.script.values[index].clone_with_heap(&self.session.heap);
                if value == JsValue::Hole {
                    return Err(self.tdz_error(name));
                }
                self.stack.push(value);
            }
            Opcode::StoreScript => {
                let index = usize::from(self.operand_u16());
                let value = self.pop();
                let old = mem::replace(&mut self.session.script.values[index], value);
                old.drop_with(&mut self.session.heap);
            }
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Exp
            | Opcode::BitAnd
            | Opcode::BitOr
            | Opcode::BitXor
            | Opcode::Shl
            | Opcode::Shr
            | Opcode::UShr => {
                let rhs = self.pop();
                let lhs = self.pop();
                let result = self.binary(op, lhs, rhs)?;
                self.stack.push(result);
            }
            Opcode::Eq | Opcode::Ne => {
                let rhs = self.pop();
                let lhs = self.pop();
                let equal = self.loose_equals(&lhs, &rhs)?;
                lhs.drop_with(&mut self.session.heap);
                rhs.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Bool(equal == (op == Opcode::Eq)));
            }
            Opcode::StrictEq | Opcode::StrictNe => {
                let rhs = self.pop();
                let lhs = self.pop();
                let equal = self.strict_equals(&lhs, &rhs);
                lhs.drop_with(&mut self.session.heap);
                rhs.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Bool(equal == (op == Opcode::StrictEq)));
            }
            Opcode::Lt | Opcode::Le | Opcode::Gt | Opcode::Ge => {
                let rhs = self.pop();
                let lhs = self.pop();
                let result = self.compare(op, lhs, rhs)?;
                self.stack.push(JsValue::Bool(result));
            }
            Opcode::InstanceOf => {
                let rhs = self.pop();
                let lhs = self.pop();
                let result = self.instance_of(&lhs, &rhs);
                lhs.drop_with(&mut self.session.heap);
                rhs.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Bool(result?));
            }
            Opcode::In => {
                let rhs = self.pop();
                let lhs = self.pop();
                let JsValue::Ref(object) = rhs else {
                    let key_shown = self.display_key_value(&lhs);
                    lhs.drop_with(&mut self.session.heap);
                    let shown = self.inspect_short(&rhs);
                    rhs.drop_with(&mut self.session.heap);
                    let (key_shown, shown) = (key_shown?, shown?);
                    return Err(self.type_error(&format!(
                        "Cannot use 'in' operator to search for '{key_shown}' in {shown}"
                    )));
                };
                let key = self.coerce_key(lhs)?;
                let found = self.has_property(object, key);
                rhs.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Bool(found));
            }
            Opcode::Neg => {
                let value = self.pop();
                let number = self.coerce_number_owned(value)?;
                self.stack.push(JsValue::Number(-number));
            }
            Opcode::Plus => {
                let value = self.pop();
                let number = self.coerce_number_owned(value)?;
                self.stack.push(JsValue::Number(number));
            }
            Opcode::Not => {
                let value = self.pop();
                let truthy = self.to_boolean(&value);
                value.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Bool(!truthy));
            }
            Opcode::BitNot => {
                let value = self.pop();
                let number = self.coerce_number_owned(value)?;
                self.stack.push(JsValue::Number(f64::from(!number::to_int32(number))));
            }
            Opcode::TypeOf => {
                let value = self.pop();
                let name = self.type_of(&value);
                value.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Atom(self.session.interns.intern_str(name)));
            }
            Opcode::IncDec => {
                let delta = self.operand_i8();
                let value = self.pop();
                let number = self.coerce_number_owned(value)?;
                self.stack.push(JsValue::Number(number + f64::from(delta)));
            }
            Opcode::Jump => {
                let offset = self.operand_i16();
                self.jump(offset);
            }
            Opcode::JumpIfFalse | Opcode::JumpIfTrue => {
                let offset = self.operand_i16();
                let value = self.pop();
                let truthy = self.to_boolean(&value);
                value.drop_with(&mut self.session.heap);
                if truthy == (op == Opcode::JumpIfTrue) {
                    self.jump(offset);
                }
            }
            Opcode::JumpIfTrueKeep | Opcode::JumpIfFalseKeep => {
                let offset = self.operand_i16();
                let truthy = self.to_boolean(self.peek(0));
                if truthy == (op == Opcode::JumpIfTrueKeep) {
                    self.jump(offset);
                } else {
                    let value = self.pop();
                    value.drop_with(&mut self.session.heap);
                }
            }
            Opcode::JumpIfNotNullishKeep => {
                let offset = self.operand_i16();
                if self.peek(0).is_nullish() {
                    let value = self.pop();
                    value.drop_with(&mut self.session.heap);
                } else {
                    self.jump(offset);
                }
            }
            Opcode::JumpIfNullishKeep => {
                let offset = self.operand_i16();
                if self.peek(0).is_nullish() {
                    self.jump(offset);
                }
            }
            Opcode::JumpIfUndefinedKeep => {
                let offset = self.operand_i16();
                if *self.peek(0) == JsValue::Undefined {
                    self.jump(offset);
                } else {
                    let value = self.pop();
                    value.drop_with(&mut self.session.heap);
                }
            }
            Opcode::GetProp => {
                let name = AtomId::from_u16(self.operand_u16());
                let object = self.pop();
                let value = self.get(&object, PropertyKey::Atom(name));
                object.drop_with(&mut self.session.heap);
                self.stack.push(value?);
            }
            Opcode::GetPropKeep => {
                let name = AtomId::from_u16(self.operand_u16());
                let object = self.peek(0).clone_with_heap(&self.session.heap);
                let value = self.get(&object, PropertyKey::Atom(name));
                object.drop_with(&mut self.session.heap);
                self.stack.push(value?);
            }
            Opcode::SetProp => {
                let name = AtomId::from_u16(self.operand_u16());
                let value = self.pop();
                let object = self.pop();
                let result = value.clone_with_heap(&self.session.heap);
                let outcome = self.set(&object, PropertyKey::Atom(name), value);
                object.drop_with(&mut self.session.heap);
                outcome?;
                self.stack.push(result);
            }
            Opcode::GetElem => {
                let key = self.pop();
                let object = self.pop();
                let key = self.coerce_key_for(&object, key);
                let value = key.and_then(|key| self.get(&object, key));
                object.drop_with(&mut self.session.heap);
                self.stack.push(value?);
            }
            Opcode::GetElemKeep => {
                let key = self.peek(0).clone_with_heap(&self.session.heap);
                let object = self.peek(1).clone_with_heap(&self.session.heap);
                let key = self.coerce_key_for(&object, key);
                let value = key.and_then(|key| self.get(&object, key));
                object.drop_with(&mut self.session.heap);
                self.stack.push(value?);
            }
            Opcode::SetElem => {
                let value = self.pop();
                let key = self.pop();
                let object = self.pop();
                let result = value.clone_with_heap(&self.session.heap);
                let outcome = match self.coerce_key_for(&object, key) {
                    Ok(key) => self.set(&object, key, value),
                    Err(err) => {
                        value.drop_with(&mut self.session.heap);
                        Err(err)
                    }
                };
                object.drop_with(&mut self.session.heap);
                outcome?;
                self.stack.push(result);
            }
            Opcode::DeleteProp => {
                let name = AtomId::from_u16(self.operand_u16());
                let object = self.pop();
                let deleted = self.delete(&object, PropertyKey::Atom(name));
                object.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Bool(deleted?));
            }
            Opcode::DeleteElem => {
                let key = self.pop();
                let object = self.pop();
                let deleted = self
                    .coerce_key_for(&object, key)
                    .and_then(|key| self.delete(&object, key));
                object.drop_with(&mut self.session.heap);
                self.stack.push(JsValue::Bool(deleted?));
            }
            Opcode::DefineOwn => {
                let name = AtomId::from_u16(self.operand_u16());
                let value = self.pop();
                let JsValue::Ref(object) = self.peek(0) else {
                    return Err(self.internal_error("DefineOwn on a non-object"));
                };
                let object = *object;
                self.define_data(object, PropertyKey::Atom(name), value);
            }
            Opcode::DefineOwnElem => {
                let value = self.pop();
                let key = self.pop();
                let JsValue::Ref(object) = self.peek(0) else {
                    return Err(self.internal_error("DefineOwnElem on a non-object"));
                };
                let object = *object;
                let key = match self.coerce_key(key) {
                    Ok(key) => key,
                    Err(err) => {
                        value.drop_with(&mut self.session.heap);
                        return Err(err);
                    }
                };
                self.define_data(object, key, value);
            }
            Opcode::DefineAccessor => {
                let which = self.operand_u8();
                let function = self.pop();
                let key = self.pop();
                let JsValue::Ref(object) = self.peek(0) else {
                    return Err(self.internal_error("DefineAccessor on a non-object"));
                };
                let object = *object;
                let key = match self.coerce_key(key) {
                    Ok(key) => key,
                    Err(err) => {
                        function.drop_with(&mut self.session.heap);
                        return Err(err);
                    }
                };
                self.define_accessor(object, key, function, which == 1);
            }
            Opcode::CopyDataProperties => {
                let source = self.pop();
                let JsValue::Ref(object) = self.peek(0) else {
                    return Err(self.internal_error("CopyDataProperties on a non-object"));
                };
                let object = *object;
                let outcome = self.copy_data_properties(object, &source);
                source.drop_with(&mut self.session.heap);
                outcome?;
            }
            Opcode::SetPrototype => {
                let proto = self.pop();
                let JsValue::Ref(object) = self.peek(0) else {
                    return Err(self.internal_error("SetPrototype on a non-object"));
                };
                let object = *object;
                match proto {
                    JsValue::Ref(proto) => self.set_prototype(object, Some(proto)),
                    JsValue::Null => self.set_prototype(object, None),
                    // any other value is ignored, as in a literal
                    other => other.drop_with(&mut self.session.heap),
                }
            }
            Opcode::NewObject => {
                let object = self.new_object();
                self.stack.push(object);
            }
            Opcode::NewArray => {
                let count = usize::from(self.operand_u16());
                let start = self.stack.len() - count;
                let items: Vec<JsValue> = self.stack.drain(start..).collect();
                let array = self.new_array(items)?;
                self.stack.push(array);
            }
            Opcode::ArrayPush => {
                let value = self.pop();
                let JsValue::Ref(array) = self.peek(0) else {
                    return Err(self.internal_error("ArrayPush on a non-array"));
                };
                let array = *array;
                self.array_push(array, value)?;
            }
            Opcode::ArrayPushHole => {
                let JsValue::Ref(array) = self.peek(0) else {
                    return Err(self.internal_error("ArrayPushHole on a non-array"));
                };
                let array = *array;
                self.array_push(array, JsValue::Hole)?;
            }
            Opcode::ArraySpread => {
                let iterable = self.pop();
                let JsValue::Ref(array) = self.peek(0) else {
                    return Err(self.internal_error("ArraySpread on a non-array"));
                };
                let array = *array;
                let items = self.iterate_to_vec(iterable)?;
                for item in items {
                    self.array_push(array, item)?;
                }
            }
            Opcode::MakeClosure => {
                let id = FunctionId::from_u16(self.operand_u16());
                let closure = self.make_closure(id, None)?;
                self.stack.push(closure);
            }
            Opcode::MakeMethod => {
                let id = FunctionId::from_u16(self.operand_u16());
                let JsValue::Ref(home) = self.peek(0) else {
                    return Err(self.internal_error("MakeMethod without a home object"));
                };
                let home = *home;
                let closure = self.make_closure(id, Some(home))?;
                self.stack.push(closure);
            }
            Opcode::LoadThis => {
                let slot = self.operand_u16();
                let value = self.local(slot).clone_with_heap(&self.session.heap);
                self.stack.push(value);
            }
            Opcode::TemplateConcat => {
                let count = usize::from(self.operand_u8());
                let start = self.stack.len() - count;
                let parts: Vec<JsValue> = self.stack.drain(start..).collect();
                let result = self.concat_strings(parts)?;
                self.stack.push(result);
            }
            Opcode::MakeClass => {
                let _ = self.operand_u8();
                return Err(self.internal_error("classes are not compiled yet"));
            }
            Opcode::Call => {
                let count = usize::from(self.operand_u8());
                let start = self.stack.len() - count;
                let args: Vec<JsValue> = self.stack.drain(start..).collect();
                let this = self.pop();
                let callee = self.pop();
                return self.call_from_bytecode(callee, this, args);
            }
            Opcode::CallSpread => {
                let array = self.pop();
                let this = self.pop();
                let callee = self.pop();
                let args = self.array_to_vec(array)?;
                return self.call_from_bytecode(callee, this, args);
            }
            Opcode::New => {
                let count = usize::from(self.operand_u8());
                let start = self.stack.len() - count;
                let args: Vec<JsValue> = self.stack.drain(start..).collect();
                let callee = self.pop();
                return self.construct_from_bytecode(callee, args);
            }
            Opcode::NewSpread => {
                let array = self.pop();
                let callee = self.pop();
                let args = self.array_to_vec(array)?;
                return self.construct_from_bytecode(callee, args);
            }
            Opcode::Return => {
                let value = self.pop();
                return Ok(Step::Return(value));
            }
            Opcode::ReturnUndefined => return Ok(Step::Return(JsValue::Undefined)),
            Opcode::SuperCall => {
                let _ = self.operand_u8();
                return Err(self.internal_error("super calls are not compiled yet"));
            }
            Opcode::Throw => {
                let value = self.pop();
                return Err(Throw::Value(value));
            }
            Opcode::ThrowError => {
                let kind = self.operand_u8();
                let message = AtomId::from_u16(self.operand_u16());
                let kind = ErrorKind::from_u8(kind).unwrap_or(ErrorKind::Error);
                let message = self.session.interns.atom(message).to_string_lossy();
                return Err(self.throw_error(kind, &message));
            }
            Opcode::PushException => {
                let value = self.pending.take().unwrap_or(JsValue::Undefined);
                self.stack.push(value);
            }
            Opcode::Rethrow => return Err(self.internal_error("Rethrow is not emitted")),
            Opcode::GetIterator => {
                let iterable = self.pop();
                let iterator = self.get_iterator(iterable)?;
                self.stack.push(iterator);
            }
            Opcode::IteratorNext => {
                let offset = self.operand_i16();
                let iterator = self.peek(0).clone_with_heap(&self.session.heap);
                let next = self.iterator_next(&iterator);
                iterator.drop_with(&mut self.session.heap);
                if let Some(value) = next? {
                    self.stack.push(value);
                } else {
                    let iterator = self.pop();
                    iterator.drop_with(&mut self.session.heap);
                    self.jump(offset);
                }
            }
            Opcode::IteratorClose => {
                let iterator = self.pop();
                let outcome = self.iterator_close(&iterator);
                iterator.drop_with(&mut self.session.heap);
                outcome?;
            }
            Opcode::ForInStart => {
                let object = self.pop();
                let iterator = self.for_in_iterator(object)?;
                self.stack.push(iterator);
            }
            Opcode::ToString => {
                let value = self.pop();
                let string = self.coerce_string_value(value)?;
                self.stack.push(string);
            }
            Opcode::ToPropertyKey => {
                let value = self.pop();
                let key = self.coerce_key(value)?;
                let value = self.key_to_value(key);
                self.stack.push(value);
            }
            Opcode::Nop => {}
            Opcode::CreateArguments => {
                let _ = self.operand_u16();
                return Err(self.internal_error("CreateArguments is not emitted"));
            }
            Opcode::RequireObjectCoercible => {
                if self.peek(0).is_nullish() {
                    let shown = self.type_of(self.peek(0));
                    return Err(self.type_error(&format!("Cannot destructure '{shown}' as it is {shown}.")));
                }
            }
        }
        Ok(Step::Continue)
    }

    /// The per-interval resource checkpoint: time and memory limits, then a
    /// cycle collection when the heap asks for one.
    fn checkpoint(&mut self) -> RunResult<()> {
        self.tracker().check_memory_time()?;
        if self.print.wants_poll() {
            self.print.poll_flush().map_err(|exc| Self::print_failure(&exc))?;
        }
        if self.session.heap.should_gc() {
            self.session.heap.collect_cycles();
        }
        Ok(())
    }

    /// A host-side print failure (a collector over its cap) ends the feed
    /// like a resource error.
    pub(crate) fn print_failure(exc: &MontyException) -> Throw {
        Throw::Resource(ResourceError::Memory {
            limit: 0,
            used: exc.message().map_or(0, str::len),
        })
    }

    // === Operands and stack ===

    fn code(&self) -> &Code {
        &self.frames.last().expect("frame").function.code
    }

    fn operand_u8(&mut self) -> u8 {
        let frame = self.frames.last_mut().expect("frame");
        let byte = frame.function.code.bytecode[frame.ip];
        frame.ip += 1;
        byte
    }

    fn operand_i8(&mut self) -> i8 {
        i8::from_le_bytes([self.operand_u8()])
    }

    fn operand_u16(&mut self) -> u16 {
        let frame = self.frames.last_mut().expect("frame");
        let bytes = &frame.function.code.bytecode[frame.ip..frame.ip + 2];
        frame.ip += 2;
        u16::from_le_bytes([bytes[0], bytes[1]])
    }

    fn operand_i16(&mut self) -> i16 {
        let frame = self.frames.last_mut().expect("frame");
        let bytes = &frame.function.code.bytecode[frame.ip..frame.ip + 2];
        frame.ip += 2;
        i16::from_le_bytes([bytes[0], bytes[1]])
    }

    fn jump(&mut self, offset: i16) {
        let frame = self.frames.last_mut().expect("frame");
        frame.ip = frame.ip.wrapping_add_signed(isize::from(offset));
    }

    pub(crate) fn pop(&mut self) -> JsValue {
        self.stack.pop().expect("operand stack underflow")
    }

    fn peek(&self, depth: usize) -> &JsValue {
        &self.stack[self.stack.len() - 1 - depth]
    }

    fn local(&self, slot: u16) -> &JsValue {
        let base = self.frames.last().expect("frame").base;
        &self.stack[base + usize::from(slot)]
    }

    fn local_mut(&mut self, slot: u16) -> &mut JsValue {
        let base = self.frames.last().expect("frame").base;
        &mut self.stack[base + usize::from(slot)]
    }

    /// The contents of the cell in `slot`.
    fn cell_value(&self, slot: u16) -> JsValue {
        match self.local(slot) {
            JsValue::Ref(cell) => match self.session.heap.get(*cell) {
                JsHeapData::Cell(value) => value.clone_with_heap(&self.session.heap),
                _ => JsValue::Undefined,
            },
            _ => JsValue::Undefined,
        }
    }

    /// Drops every stack value from `len` up.
    fn clear_stack(&mut self, len: usize) {
        while self.stack.len() > len {
            let value = self.stack.pop().expect("stack");
            value.drop_with(&mut self.session.heap);
        }
    }

    // === Errors ===

    /// Allocates a builtin error and wraps it as a throw.
    pub fn throw_error(&mut self, kind: ErrorKind, message: &str) -> Throw {
        let error = self.new_error(kind, message);
        Throw::Value(error)
    }

    pub fn type_error(&mut self, message: &str) -> Throw {
        self.throw_error(ErrorKind::TypeError, message)
    }

    pub fn range_error(&mut self, message: &str) -> Throw {
        self.throw_error(ErrorKind::RangeError, message)
    }

    pub fn syntax_error(&mut self, message: &str) -> Throw {
        self.throw_error(ErrorKind::SyntaxError, message)
    }

    fn reference_error(&mut self, name: AtomId) -> Throw {
        let name = self.session.interns.atom(name).to_string_lossy();
        self.throw_error(ErrorKind::ReferenceError, &format!("{name} is not defined"))
    }

    fn tdz_error(&mut self, name: AtomId) -> Throw {
        let name = self.session.interns.atom(name).to_string_lossy();
        self.throw_error(
            ErrorKind::ReferenceError,
            &format!("Cannot access '{name}' before initialization"),
        )
    }

    /// A VM invariant failed: surfaces as a generic `Error` so the feed ends
    /// with a message rather than a panic.
    pub fn internal_error(&mut self, message: &str) -> Throw {
        self.throw_error(ErrorKind::Error, &format!("internal error: {message}"))
    }

    /// Interns `text` as an atom value.
    pub fn atom_value(&mut self, text: &str) -> JsValue {
        JsValue::Atom(self.session.interns.intern_str(text))
    }

    /// The text of a string value, or `None` for anything else.
    pub fn string_of(&self, value: &JsValue) -> Option<JsString> {
        match value {
            JsValue::Atom(atom) => Some(self.session.interns.atom(*atom).clone()),
            JsValue::Str(id) => match self.session.heap.get(*id) {
                JsHeapData::String(string) => Some(string.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Allocates a string value, charging the tracker for its storage.
    pub fn alloc_string(&mut self, string: JsString) -> RunResult<JsValue> {
        self.tracker().check_allocation(string.len() * 2 + 32)?;
        Ok(JsValue::Str(self.session.heap.allocate(JsHeapData::String(string))))
    }

    /// Reads a heap object; `None` when the id holds something else.
    pub fn object(&self, id: HeapId) -> Option<&JsObject> {
        match self.session.heap.get(id) {
            JsHeapData::Object(object) => Some(object),
            _ => None,
        }
    }

    pub fn object_mut(&mut self, id: HeapId) -> Option<&mut JsObject> {
        match self.session.heap.get_mut(id) {
            JsHeapData::Object(object) => Some(object),
            _ => None,
        }
    }
}

/// What one instruction did to control flow.
enum Step {
    Continue,
    /// The frame returned this value.
    Return(JsValue),
}

impl ContainsHeap for Vm<'_> {
    type Data = JsHeapData;

    fn heap(&self) -> &Heap {
        &self.session.heap
    }

    fn heap_mut(&mut self) -> &mut Heap {
        &mut self.session.heap
    }
}
