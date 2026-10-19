//! `monty-js subprocess`: protocol child mode over the JavaScript engine.
//!
//! The framed-stdio loop is [`monty_proto::worker::stdio`]; this module runs
//! it for [`JsEngine`](crate::engine::JsEngine) and applies the session's hard
//! memory limit to the allocator this binary declares.

use std::process::ExitCode;

use monty_proto::worker::{Child, stdio::run_worker_thread};
use monty_types::memory_limit_with_headroom;

use crate::engine::JsEngine;

/// Serves the protocol on a worker thread until the parent is done.
pub(crate) fn run() -> ExitCode {
    run_worker_thread(apply_memory_limit)
}

/// Applies the hard memory limit of whatever session the child now holds,
/// after every request (see `monty-runtime` for why the child's state, not
/// the request, decides).
fn apply_memory_limit(child: &Child<JsEngine>) {
    let budget = child.session_budget();
    let hard_memory_limit = memory_limit_with_headroom(budget.max_memory, budget.type_check);
    monty_alloc::set_hard_limit(hard_memory_limit).expect("monty-js-runtime must install LimitedAllocator globally");
}
