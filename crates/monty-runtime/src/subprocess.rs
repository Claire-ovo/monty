//! `monty subprocess`: protocol child mode.
//!
//! The framed-stdio loop itself is [`monty_proto::worker::stdio`]; this
//! module runs it for the Python engine and applies the session's hard
//! memory limit to the allocator this binary declares.

use std::process::ExitCode;

use monty_proto::worker::{python::PythonChild, stdio::run_worker_thread};
use monty_types::memory_limit_with_headroom;

/// Serves the protocol on a worker thread until the parent is done.
pub(crate) fn run() -> ExitCode {
    run_worker_thread(apply_memory_limit)
}

/// Applies the hard memory limit of whatever session the child now holds,
/// after every request.
///
/// Reading the child's state rather than the request is what keeps the limit
/// honest: a rejected `Configure` changes nothing, a `Load` brings the dump's
/// limits capped by the destination policy, and `Reset` ends the session. It
/// lives in the binary rather than in the shared loop because each entry point
/// declares its own allocator: the wasm worker does the same thing in its own
/// turn loop.
fn apply_memory_limit(child: &PythonChild) {
    let budget = child.session_budget();
    let hard_memory_limit = memory_limit_with_headroom(budget.max_memory, budget.type_check);
    monty_alloc::set_hard_limit(hard_memory_limit).expect("monty-runtime must install LimitedAllocator globally");
}
