//! The framed-stdio shell every native worker binary runs: requests in on
//! stdin, events out on stdout, until EOF, `Shutdown` or a fatal error.
//!
//! Crash isolation is the point of this mode: the parent treats a child that
//! exits (or EOFs) *without* a `FatalError` event as crashed — stack overflows
//! and allocator aborts produce no final frame. Stdout carries only protocol
//! frames; diagnostics go to stderr.

use std::{io, panic, process::ExitCode, thread};

use super::{Child, Engine, EventSink, HandleOutcome, fatal_error_event, protocol_violation};
use crate::{FrameError, FrameReader, pb, write_frame};

/// BSD `sysexits.h` "remote error in protocol" — the frame stream desynchronized, or
/// an event too large to frame left the response unsendable.
const EX_PROTOCOL: u8 = 76;

/// Native stack for the worker thread. The interpreters' container `repr`,
/// `==` and value exporters recurse once per nesting level, up to the
/// 1000-frame recursion limit: that fits the 8 MiB main thread of Linux and
/// macOS but not Windows' 1 MiB. A fixed size gives every OS the same budget;
/// the memory is reserved, not committed.
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Runs [`serve_stdio`] on a thread with [`WORKER_STACK_SIZE`] of stack and
/// returns its exit code. A panic there has already written its `FatalError`
/// frame in the panic hook, so it exits with 101 like an unwound main thread.
///
/// `after_request` runs after every request with the child's current state;
/// binaries use it to apply the session's hard memory limit to their
/// allocator, which only a binary can declare.
///
/// # Panics
/// If the worker thread cannot be spawned.
pub fn run_worker_thread<E: Engine + Send + 'static>(after_request: impl FnMut(&Child<E>) + Send + 'static) -> ExitCode
where
    E::Repl: Send,
    E::Progress: Send,
{
    thread::Builder::new()
        .name("monty-worker".to_owned())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || serve_stdio(after_request))
        .expect("failed to spawn the worker thread")
        .join()
        .unwrap_or(ExitCode::from(101))
}

/// The child loop: one request in, its events out, until EOF, `Shutdown` or
/// a fatal error. Installs the panic hook that turns an ordinary panic into a
/// last-gasp `FatalError` frame.
pub fn serve_stdio<E: Engine>(mut after_request: impl FnMut(&Child<E>)) -> ExitCode {
    install_panic_hook();
    let mut reader = FrameReader::new(io::stdin().lock());
    let mut child = Child::<E>::default();
    let mut sink = StdoutSink;

    loop {
        match reader.read::<pb::ParentRequest>() {
            Ok(Some(request)) => {
                let outcome = child.handle(request, &mut sink);
                after_request(&child);
                match outcome {
                    Ok(HandleOutcome::Continue) => {}
                    Ok(HandleOutcome::Shutdown) => return ExitCode::SUCCESS,
                    // the child emitted a FatalError (e.g. version skew) and cannot
                    // keep serving — exit non-zero so the parent sees a clean cause
                    Ok(HandleOutcome::Fatal) => return ExitCode::from(4),
                    // an oversize event was rejected before any bytes hit the
                    // wire, so the stream is still in sync and the parent can
                    // receive a parseable last gasp
                    Err(FrameError::FrameTooLarge { len, max }) => {
                        fatal(
                            &child,
                            &mut sink,
                            &format!("response frame of {len} bytes exceeds maximum of {max} bytes"),
                        );
                        return ExitCode::from(EX_PROTOCOL);
                    }
                    // writing to stdout failed: the parent is gone, nothing left to do
                    Err(_) => return ExitCode::from(3),
                }
            }
            // clean EOF at a frame boundary: the parent closed stdin
            Ok(None) => return ExitCode::SUCCESS,
            // the frame arrived intact but its payload didn't decode — this
            // includes values failing semantic validation (bad dates, unknown
            // enum names), which happens during decode. The stream is still
            // in sync, so answer with a turn-ending error and keep serving.
            Err(FrameError::Decode(err)) => {
                if sink
                    .send(&protocol_violation(&format!("malformed request: {err}")))
                    .is_err()
                {
                    return ExitCode::from(3);
                }
            }
            Err(err) => {
                // the stream is desynchronized — unrecoverable by design
                fatal(&child, &mut sink, &format!("malformed request frame: {err}"));
                return ExitCode::from(EX_PROTOCOL);
            }
        }
    }
}

/// Writes framed child events to stdout.
///
/// Framing is stateless and `Stdout` handles share one global buffer, so a
/// fresh handle per write is safe.
struct StdoutSink;

impl EventSink for StdoutSink {
    fn send(&mut self, event: &pb::ChildEvent) -> Result<(), FrameError> {
        write_frame(&mut io::stdout(), event)
    }
}

/// Emits a best-effort `FatalError` event, duplicated to stderr. Used only for
/// unrecoverable conditions detected by the shell — the child exits right
/// after.
fn fatal<E: Engine>(child: &Child<E>, sink: &mut impl EventSink, message: &str) {
    eprintln!("monty subprocess fatal error: {message}");
    let _ = sink.send(&child.fatal_event(message));
}

/// Installs a panic hook that emits a best-effort `FatalError` frame before
/// the default unwind, giving the parent a parseable last gasp for ordinary
/// panics. Hard crashes (stack overflow, allocator abort) bypass this — the
/// parent's contract is "exit without FatalError == crash".
fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // stdout's lock is reentrant on the same thread, and if the panic
        // interrupted a write its buffer may hold a partial frame we cannot
        // complete — a corrupt tail is fine, the parent already treats it as
        // a crash. The hook has no `Child` in scope, so the event is unstamped.
        let _ = write_frame(
            &mut io::stdout(),
            &fatal_error_event(&format!("child panicked: {info}")),
        );
        default_hook(info);
    }));
}
