//! Explicit release of heap references: the [`DropWithContext`] contract, the
//! [`DropGuard`] that enforces it on every code path, and the `defer_drop!`
//! macros built on the guard.

use std::{
    mem::ManuallyDrop,
    ptr::addr_of,
    vec::{Drain, IntoIter},
};

use smallvec::SmallVec;

/// Cleanup for types holding heap (and possibly VM-side) references that must be
/// released explicitly — Rust's `Drop` cannot, since it has no heap access.
///
/// `C` is whatever cleanup context the caller has on hand: a [`Heap`](crate::Heap),
/// a [`HeapReader`](crate::HeapReader), a language's VM or encoder. The bound on
/// each impl states the capability the value needs: heap-only values bound `C` by
/// [`ContainsHeap`](crate::ContainsHeap) (satisfied by all of those), while values
/// holding VM-side state bound `C` by the language's own context trait.
///
/// **All implementers must be cleaned up on every code path** — not just the happy
/// path, but early returns via `?`, `continue`, conditional branches, etc. A missed
/// call leaks reference counts. Prefer [`defer_drop!`](crate::defer_drop) or
/// [`DropGuard`] to guarantee cleanup automatically rather than inserting manual
/// calls in every branch.
pub trait DropWithContext<C: ?Sized> {
    /// Consume `self`, releasing every heap/VM reference it owns through `ctx`.
    ///
    /// Implementations owning raw `HeapId`s may call `Heap::dec_ref` here. Callers
    /// should prefer a guard unless this is a simple, linear cleanup path.
    fn drop_with(self, ctx: &mut C);
}

impl<C, U: DropWithContext<C>> DropWithContext<C> for Option<U> {
    #[inline]
    fn drop_with(self, ctx: &mut C) {
        if let Some(value) = self {
            value.drop_with(ctx);
        }
    }
}

impl<C, U: DropWithContext<C>> DropWithContext<C> for Vec<U> {
    fn drop_with(self, ctx: &mut C) {
        for value in self {
            value.drop_with(ctx);
        }
    }
}

impl<C, A: smallvec::Array> DropWithContext<C> for SmallVec<A>
where
    A::Item: DropWithContext<C>,
{
    fn drop_with(self, ctx: &mut C) {
        for value in self {
            value.drop_with(ctx);
        }
    }
}

impl<C, U: DropWithContext<C>> DropWithContext<C> for IntoIter<U> {
    fn drop_with(self, ctx: &mut C) {
        for value in self {
            value.drop_with(ctx);
        }
    }
}

impl<C, U: DropWithContext<C>> DropWithContext<C> for Drain<'_, U> {
    fn drop_with(self, ctx: &mut C) {
        for value in self {
            value.drop_with(ctx);
        }
    }
}

impl<C, U: DropWithContext<C>, const N: usize> DropWithContext<C> for [U; N] {
    fn drop_with(self, ctx: &mut C) {
        for value in self {
            // Qualified call: a language value's inherent `drop_with` would
            // otherwise shadow the trait method and demand its own bound here.
            DropWithContext::drop_with(value, ctx);
        }
    }
}

impl<C, U: DropWithContext<C>, V: DropWithContext<C>> DropWithContext<C> for (U, V) {
    fn drop_with(self, ctx: &mut C) {
        let (left, right) = self;
        left.drop_with(ctx);
        right.drop_with(ctx);
    }
}

/// RAII guard that ensures a [`DropWithContext`] value is cleaned up on every code path.
///
/// The guard's `Drop` impl calls [`DropWithContext::drop_with`] automatically, so
/// cleanup happens whether the scope exits normally, via `?`, `continue`, early return,
/// or any other branch. This eliminates the need to manually insert `drop_with`
/// calls in every branch.
///
/// `C` is the cleanup context (`Heap` / `HeapReader` / VM / encoder); it is *not*
/// bounded here, so a single guard type serves both heap-only values and
/// VM-state holders — the `V: DropWithContext<C>` bound is what constrains the
/// pairing, exactly as on a bare `drop_with` call.
///
/// On the normal path, the guarded value can be borrowed via [`as_parts`](Self::as_parts) /
/// [`as_parts_mut`](Self::as_parts_mut), or reclaimed via [`into_inner`](Self::into_inner) /
/// [`into_parts`](Self::into_parts) (which consume the guard without dropping the value).
///
/// Prefer the [`defer_drop!`](crate::defer_drop) macro for the common case where you
/// just need to ensure a value is dropped at scope exit. Use `DropGuard` directly when
/// you need to conditionally reclaim the value (e.g. push it back onto the stack on
/// success) or need mutable access to both the value and context through
/// [`as_parts_mut`](Self::as_parts_mut).
pub struct DropGuard<'a, C, V: DropWithContext<C>> {
    // manually dropped because it needs to be dropped by move.
    value: ManuallyDrop<V>,
    ctx: &'a mut C,
}

impl<'a, C, V: DropWithContext<C>> DropGuard<'a, C, V> {
    /// Creates a new `DropGuard` for the given value and context.
    #[inline]
    pub fn new(value: V, ctx: &'a mut C) -> Self {
        Self {
            value: ManuallyDrop::new(value),
            ctx,
        }
    }

    /// Consumes the guard and returns the contained value without dropping it.
    ///
    /// Use this when the value should survive beyond the guard's scope (e.g. returning
    /// a computed result from a function that used the guard for error-path safety).
    #[inline]
    pub fn into_inner(self) -> V {
        let mut this = ManuallyDrop::new(self);
        // SAFETY: [DH] - `ManuallyDrop::new(self)` prevents `Drop` on self, so we can take the value out
        unsafe { ManuallyDrop::take(&mut this.value) }
    }

    /// Borrows the value (immutably) and context (mutably) out of the guard.
    ///
    /// This is what [`defer_drop!`](crate::defer_drop) calls internally. The returned
    /// references are tied to the guard's lifetime, so the value cannot escape.
    #[inline]
    pub fn as_parts(&mut self) -> (&V, &mut C) {
        (&self.value, self.ctx)
    }

    /// Borrows the value (mutably) and context (mutably) out of the guard.
    ///
    /// This is what [`defer_drop_mut!`](crate::defer_drop_mut) calls internally. Use this
    /// when the value needs to be mutated in place (e.g. advancing an iterator, swapping
    /// during min/max).
    #[inline]
    pub fn as_parts_mut(&mut self) -> (&mut V, &mut C) {
        (&mut self.value, self.ctx)
    }

    /// Consumes the guard and returns the value and context separately, without dropping.
    ///
    /// Use this when you need to reclaim both the value *and* the context reference — for
    /// example, to push the value back onto the VM stack via the heap owner.
    #[inline]
    pub fn into_parts(self) -> (V, &'a mut C) {
        let mut this = ManuallyDrop::new(self);
        // SAFETY: [DH] - `ManuallyDrop` prevents `Drop` on self, so we can recover the parts
        unsafe { (ManuallyDrop::take(&mut this.value), addr_of!(this.ctx).read()) }
    }

    /// Borrows just the context out of the guard
    #[inline]
    pub fn ctx(&mut self) -> &mut C {
        self.ctx
    }
}

impl<C, V: DropWithContext<C>> Drop for DropGuard<'_, C, V> {
    fn drop(&mut self) {
        // SAFETY: [DH] - value is never manually dropped until this point
        unsafe { ManuallyDrop::take(&mut self.value) }.drop_with(self.ctx);
    }
}

/// The preferred way to ensure a [`DropWithContext`] value is cleaned up on every code path.
///
/// Creates a [`DropGuard`] and immediately rebinds `$value` as `&V` and `$ctx` as
/// `&mut C` via [`DropGuard::as_parts`]. The original owned value is moved into the
/// guard, which will call [`DropWithContext::drop_with`] when scope exits — whether
/// that's normal completion, early return via `?`, `continue`, or any other branch.
///
/// Beyond safety, this is often much more concise than inserting `drop_with` calls
/// in every branch of complex control flow. For mutable access to the value, use
/// [`defer_drop_mut!`](crate::defer_drop_mut).
///
/// # Limitation
///
/// The macro rebinds `$ctx` as a new `let` binding, so it cannot be used when `$ctx`
/// is `self`. In `&mut self` methods, first assign `let this = self;` and pass `this`.
#[macro_export]
macro_rules! defer_drop {
    ($value:ident, $ctx:ident) => {
        let mut _guard = $crate::DropGuard::new($value, $ctx);
        #[allow(
            clippy::allow_attributes,
            reason = "the reborrowed parts may not both be used in every case, so allow unused vars to avoid warnings"
        )]
        #[allow(unused_variables)]
        let ($value, $ctx) = _guard.as_parts();
    };
}

/// Like [`defer_drop!`](crate::defer_drop), but rebinds `$value` as `&mut V` via [`DropGuard::as_parts_mut`].
///
/// Use this when the value needs to be mutated in place — for example, advancing an
/// iterator with `for_next()`, or swapping values during a min/max comparison.
#[macro_export]
macro_rules! defer_drop_mut {
    ($value:ident, $ctx:ident) => {
        let mut _guard = $crate::DropGuard::new($value, $ctx);
        #[allow(
            clippy::allow_attributes,
            reason = "the reborrowed parts may not both be used in every case, so allow unused vars to avoid warnings"
        )]
        #[allow(unused_variables)]
        let ($value, $ctx) = _guard.as_parts_mut();
    };
}
