//! Suspendable frames: a `function*` call parks its frame in a heap object
//! after the prologue, and each `next`/`return`/`throw` puts the frame back
//! on the stack with the sent value and a [`ResumeMode`], runs it to the
//! next suspension point, and parks it again. An async function's frame is
//! parked the same way in a `Coroutine` (`await`) and resumed by promise
//! jobs. `yield*` forwards each resumption to an inner iterator through
//! [`Vm::delegate_step`].

use std::{mem, sync::Arc};

use monty_types::ResourceError;

use super::{Construct, Frame, Outcome, Vm};
use crate::{
    error::{RunResult, Throw},
    heap::{Coroutine, GeneratorState, HeapId, JsHeapData, JsObject, ObjectClass, SavedFrame},
    intern::StaticAtom,
    value::JsValue,
};

/// Which generator method resumed a frame; pushed as a number for
/// `ResumeGenerator` and `DelegateStep` to dispatch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeMode {
    Next = 0,
    Throw = 1,
    Return = 2,
}

impl ResumeMode {
    /// Decodes the mode a suspension point left on the stack.
    pub fn from_value(value: &JsValue) -> Option<Self> {
        let JsValue::Number(n) = value else {
            return None;
        };
        // the VM pushed one of three exact small integers
        match n.to_bits() {
            bits if bits == 0.0_f64.to_bits() => Some(Self::Next),
            bits if bits == 1.0_f64.to_bits() => Some(Self::Throw),
            bits if bits == 2.0_f64.to_bits() => Some(Self::Return),
            _ => None,
        }
    }
}

impl Vm<'_> {
    /// `generator.next(value)` / `.throw(value)` / `.return(value)`: runs the
    /// parked frame to its next suspension point and returns the iterator
    /// result; a finished generator answers without running.
    pub(crate) fn resume_generator(&mut self, id: HeapId, mode: ResumeMode, value: JsValue) -> RunResult<JsValue> {
        let Some(ObjectClass::Generator(state)) = self.object_mut(id).map(|o| &mut o.class) else {
            value.drop_with(&mut self.session.heap);
            return Err(self.internal_error("resuming a non-generator"));
        };
        let state = mem::replace(&mut **state, GeneratorState::Running);
        let saved = match state {
            GeneratorState::Completed => {
                self.set_generator_state(id, GeneratorState::Completed);
                return match mode {
                    ResumeMode::Next => {
                        value.drop_with(&mut self.session.heap);
                        Ok(self.iter_result(JsValue::Undefined, true))
                    }
                    ResumeMode::Return => Ok(self.iter_result(value, true)),
                    ResumeMode::Throw => Err(Throw::Value(value)),
                };
            }
            GeneratorState::Running => {
                value.drop_with(&mut self.session.heap);
                return Err(self.type_error("Generator is already running"));
            }
            GeneratorState::Suspended(saved) => saved,
        };
        let depth = self.frames.len();
        let outcome = self.enter_nested(|vm| {
            vm.restore_frame(id, saved, value, mode)?;
            vm.run(depth)
        });
        match outcome {
            // the frame parked itself again on the way out
            Ok(Outcome::Yield(result)) => Ok(result),
            Ok(Outcome::Return(value)) => {
                self.set_generator_state(id, GeneratorState::Completed);
                Ok(self.iter_result(value, true))
            }
            Err(err) => {
                self.set_generator_state(id, GeneratorState::Completed);
                Err(err)
            }
        }
    }

    /// Resumes an async function's parked frame with a settlement; it runs
    /// to its next `await` (which schedules the following resumption) or to
    /// the end (which settles its promise inside the run loop).
    pub(crate) fn resume_coroutine(&mut self, id: HeapId, mode: ResumeMode, value: JsValue) -> RunResult<()> {
        let Some(ObjectClass::Coroutine(coroutine)) = self.object_mut(id).map(|o| &mut o.class) else {
            value.drop_with(&mut self.session.heap);
            return Err(self.internal_error("resuming a non-coroutine"));
        };
        let GeneratorState::Suspended(saved) = mem::replace(&mut coroutine.state, GeneratorState::Running) else {
            value.drop_with(&mut self.session.heap);
            return Err(self.internal_error("resuming an async function that is not suspended"));
        };
        let depth = self.frames.len();
        let outcome = self.enter_nested(|vm| {
            vm.restore_frame(id, saved, value, mode)?;
            vm.run(depth)
        });
        match outcome {
            Ok(Outcome::Return(value) | Outcome::Yield(value)) => {
                value.drop_with(&mut self.session.heap);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Marks the top frame as an async function's: its coroutine object (a
    /// reference the frame owns) holds the promise the call returns.
    pub(crate) fn start_coroutine(&mut self) {
        let promise = self.new_promise();
        let coroutine = self.session.heap.allocate(JsHeapData::Object(JsObject::with_class(
            None,
            ObjectClass::Coroutine(Box::new(Coroutine {
                state: GeneratorState::Running,
                promise,
            })),
        )));
        let frame = self.frames.last_mut().expect("frame");
        frame.generator = Some(coroutine);
        frame.async_initial = true;
    }

    /// Whether `id` is an async function's activation.
    pub(super) fn is_coroutine(&self, id: HeapId) -> bool {
        matches!(self.object(id).map(|o| &o.class), Some(ObjectClass::Coroutine(_)))
    }

    /// The promise an async function's call returned.
    pub(super) fn coroutine_promise(&self, id: HeapId) -> HeapId {
        match self.object(id).map(|o| &o.class) {
            Some(ObjectClass::Coroutine(coroutine)) => coroutine.promise,
            _ => unreachable!("checked by `is_coroutine`"),
        }
    }

    pub(super) fn set_coroutine_state(&mut self, id: HeapId, state: GeneratorState) {
        if let Some(ObjectClass::Coroutine(coroutine)) = self.object_mut(id).map(|o| &mut o.class) {
            coroutine.state = state;
        }
    }

    fn set_generator_state(&mut self, id: HeapId, state: GeneratorState) {
        match self.object_mut(id).map(|o| &mut o.class) {
            Some(ObjectClass::Generator(slot)) => **slot = state,
            Some(ObjectClass::Coroutine(coroutine)) => coroutine.state = state,
            _ => {}
        }
    }

    /// A generator function's call result: the generator object holding the
    /// frame that just finished its prologue, inheriting from the function's
    /// `prototype` property. The frame owns a reference to it until it is parked.
    pub(super) fn create_generator(&mut self) -> RunResult<JsValue> {
        let frame = self.frames.last().expect("frame");
        let Some(callee) = frame.callee else {
            return Err(self.internal_error("generator frame without a callee"));
        };
        let prototype = self.get_atom(&JsValue::Ref(callee), StaticAtom::Prototype.id())?;
        let proto = match prototype {
            JsValue::Ref(proto) => proto,
            other => {
                other.drop_with(&mut self.session.heap);
                let proto = self.session.realm.generator_prototype;
                self.session.heap.inc_ref(proto);
                proto
            }
        };
        let generator = self.session.heap.allocate(JsHeapData::Object(JsObject::with_class(
            Some(proto),
            ObjectClass::Generator(Box::new(GeneratorState::Running)),
        )));
        self.suspend_frame(generator);
        Ok(JsValue::Ref(generator))
    }

    /// Parks the top frame in `owner` (a generator or coroutine): its stack
    /// region moves into the heap and the frame's reference to the callee
    /// goes with it. The frame's own reference to `owner` stays for the
    /// caller to release once it has finished with the object.
    pub(super) fn suspend_frame(&mut self, owner: HeapId) {
        let frame = self.frames.pop().expect("frame");
        let values: Vec<JsValue> = self.stack.drain(frame.base..).collect();
        frame.new_target.drop_with(&mut self.session.heap);
        let saved = SavedFrame {
            function: frame.function.id,
            callee: frame.callee,
            ip: u32::try_from(frame.ip).expect("bytecode offsets fit u32"),
            values,
        };
        self.set_generator_state(owner, GeneratorState::Suspended(saved));
    }

    /// Puts a parked frame back on the stack to continue at its saved
    /// instruction, with `value` and the resume `mode` pushed for the
    /// `ResumeGenerator` that follows every suspension point.
    fn restore_frame(&mut self, owner: HeapId, saved: SavedFrame, value: JsValue, mode: ResumeMode) -> RunResult<()> {
        let SavedFrame {
            function,
            callee,
            ip,
            values,
        } = saved;
        let function = Arc::clone(self.session.interns.function(function));
        if let Err(err) = self.tracker().check_recursion_depth(self.frames.len() + 1) {
            if let Some(callee) = callee {
                self.session.heap.dec_ref(callee);
            }
            self.drop_all(value, values);
            return Err(match err {
                ResourceError::Recursion { .. } => self.range_error("Maximum call stack size exceeded"),
                other => Throw::Resource(other),
            });
        }
        let base = self.stack.len();
        let operands = base + usize::from(function.locals);
        self.stack.extend(values);
        self.session.heap.inc_ref(owner);
        self.frames.push(Frame {
            function,
            ip: ip as usize,
            op_start: ip as usize,
            base,
            operands,
            callee,
            new_target: JsValue::Undefined,
            construct: Construct::Call,
            generator: Some(owner),
            async_initial: false,
        });
        self.stack.push(value);
        self.stack.push(JsValue::Number(f64::from(mode as u8)));
        Ok(())
    }

    /// One `yield*` step over `iter sent mode` on the stack: forwards the
    /// resumption to the inner iterator's `next`/`throw`/`return`. Leaves
    /// `iter result` and returns `false` to yield the result as is, or pops
    /// the iterator, leaves `value mode` and returns `true` once it is done.
    pub(super) fn delegate_step(&mut self) -> RunResult<bool> {
        let mode_value = self.pop();
        let sent = self.pop();
        let Some(mode) = ResumeMode::from_value(&mode_value) else {
            sent.drop_with(&mut self.session.heap);
            return Err(self.internal_error("bad resume mode"));
        };
        let iterator = self.stack.last().expect("iterator").clone_with_heap(&self.session.heap);
        let method_name = match mode {
            ResumeMode::Next => StaticAtom::Next,
            ResumeMode::Throw => StaticAtom::Throw,
            ResumeMode::Return => StaticAtom::Return,
        };
        let method = self.get_atom(&iterator, method_name.id())?;
        if method.is_nullish() {
            method.drop_with(&mut self.session.heap);
            return match mode {
                ResumeMode::Next => {
                    sent.drop_with(&mut self.session.heap);
                    iterator.drop_with(&mut self.session.heap);
                    Err(self.type_error("undefined is not a function"))
                }
                // no `throw`: the inner iterator is closed and the outer gets a TypeError
                ResumeMode::Throw => {
                    sent.drop_with(&mut self.session.heap);
                    let closed = self.iterator_close(&iterator);
                    iterator.drop_with(&mut self.session.heap);
                    closed?;
                    Err(self.type_error("The iterator does not provide a 'throw' method."))
                }
                // no `return`: the outer generator returns the sent value itself
                ResumeMode::Return => {
                    iterator.drop_with(&mut self.session.heap);
                    let iterator = self.pop();
                    iterator.drop_with(&mut self.session.heap);
                    self.stack.push(sent);
                    self.stack.push(JsValue::Number(f64::from(ResumeMode::Return as u8)));
                    Ok(true)
                }
            };
        }
        let result = self.call(method, iterator, vec![sent])?;
        if !result.is_object() {
            let shown = self.inspect_short(&result)?;
            result.drop_with(&mut self.session.heap);
            return Err(self.type_error(&format!("Iterator result {shown} is not an object")));
        }
        let done = self.get_atom(&result, StaticAtom::Done.id())?;
        let is_done = self.to_boolean(&done);
        done.drop_with(&mut self.session.heap);
        if is_done {
            let value = self.get_atom(&result, StaticAtom::Value.id())?;
            result.drop_with(&mut self.session.heap);
            let iterator = self.pop();
            iterator.drop_with(&mut self.session.heap);
            self.stack.push(value);
            // a finished `return` completes the outer generator; `next` and `throw` continue past the `yield*`
            let outer_mode = if mode == ResumeMode::Return {
                ResumeMode::Return
            } else {
                ResumeMode::Next
            };
            self.stack.push(JsValue::Number(f64::from(outer_mode as u8)));
            Ok(true)
        } else {
            self.stack.push(result);
            Ok(false)
        }
    }
}
