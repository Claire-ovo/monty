//! Integration tests for `monty-js subprocess`: spawn the real binary and
//! drive it over the wire protocol the Python worker speaks.

use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use monty_proto::{
    FrameReader, PROTOCOL_VERSION, WireFunctionCall, ext_result_to_proto, future_results_to_proto,
    named_values_to_proto, pb, write_frame,
};
use monty_types::{ExtFunctionResult, MontyObject, NameLookupResult, NamedValues};

fn configure() -> pb::Configure {
    pb::Configure {
        script_name: "main.js".to_owned(),
        limits: None,
        type_check: false,
        type_check_stubs: None,
        monty_version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol_version: PROTOCOL_VERSION,
        assert_message_annotations: None,
        ..Default::default()
    }
}

/// A spawned `monty-js subprocess` child with framed pipes.
struct ChildProc {
    _child: Child,
    writer: ChildStdin,
    reader: FrameReader<ChildStdout>,
}

impl ChildProc {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_monty-js"))
            .arg("subprocess")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn monty-js subprocess");
        let writer = child.stdin.take().expect("child stdin");
        let reader = FrameReader::new(child.stdout.take().expect("child stdout"));
        Self {
            _child: child,
            writer,
            reader,
        }
    }

    fn send(&mut self, kind: pb::parent_request::Kind) {
        write_frame(
            &mut self.writer,
            &pb::ParentRequest {
                kind: Some(kind),
                trace_parent: None,
            },
        )
        .expect("failed to write request");
    }

    fn recv(&mut self) -> pb::child_event::Kind {
        self.reader
            .read::<pb::ChildEvent>()
            .expect("failed to read event")
            .expect("unexpected EOF from child")
            .kind
            .expect("event has no kind")
    }

    /// Reads until the turn-ending event, collecting streamed prints.
    fn recv_turn(&mut self) -> (Vec<pb::Print>, pb::child_event::Kind) {
        let mut prints = Vec::new();
        loop {
            match self.recv() {
                pb::child_event::Kind::Print(print) => prints.push(print),
                other => return (prints, other),
            }
        }
    }

    fn create_repl(&mut self) {
        self.create_repl_with(configure());
    }

    fn create_repl_with(&mut self, create: pb::Configure) {
        self.send(pb::parent_request::Kind::Configure(create));
        match self.recv() {
            pb::child_event::Kind::Ok(_) => {}
            other => panic!("expected Ok for Configure, got {other:?}"),
        }
    }

    fn feed(&mut self, code: &str) -> (Vec<pb::Print>, pb::child_event::Kind) {
        self.feed_with(code, NamedValues::new())
    }

    fn feed_with(&mut self, code: &str, inputs: NamedValues) -> (Vec<pb::Print>, pb::child_event::Kind) {
        let (inputs, values) = named_values_to_proto(inputs);
        self.send(pb::parent_request::Kind::Feed(pb::Feed {
            code: code.to_owned(),
            inputs,
            values: Some(values),
            skip_type_check: false,
            cwd: "/".to_owned(),
        }));
        self.recv_turn()
    }

    #[track_caller]
    fn feed_complete(&mut self, code: &str) -> MontyObject {
        let (_, event) = self.feed(code);
        expect_complete(event)
    }

    /// Answers a suspended host call.
    fn resume_call(&mut self, call_id: u32, result: ExtFunctionResult) -> (Vec<pb::Print>, pb::child_event::Kind) {
        let (result, values) = ext_result_to_proto(result);
        self.send(pb::parent_request::Kind::ResumeCall(pb::ResumeCall {
            call_id,
            result: Some(result),
            values,
        }));
        self.recv_turn()
    }

    /// Answers a suspended name lookup.
    fn resume_name_lookup(&mut self, result: NameLookupResult) -> (Vec<pb::Print>, pb::child_event::Kind) {
        self.send(pb::parent_request::Kind::ResumeNameLookup(result.into()));
        self.recv_turn()
    }
}

#[track_caller]
fn expect_function_call(event: pb::child_event::Kind) -> WireFunctionCall {
    match event {
        pb::child_event::Kind::FunctionCall(call) => call,
        other => panic!("expected FunctionCall, got {other:?}"),
    }
}

/// The positional arguments of a suspended call.
fn call_args(call: &WireFunctionCall) -> Vec<MontyObject> {
    call.clone()
        .into_call_args()
        .expect("valid call arguments")
        .args()
        .map(|arg| arg.to_owned())
        .collect()
}

#[track_caller]
fn expect_complete(event: pb::child_event::Kind) -> MontyObject {
    match event {
        pb::child_event::Kind::Complete(complete) => complete.try_into().expect("decodable value"),
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[track_caller]
fn expect_error(event: pb::child_event::Kind) -> pb::RaisedException {
    match event {
        pb::child_event::Kind::Error(error) => error.exception.expect("error carries an exception"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn session_state_persists_across_feeds() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    child.feed_complete("let total = 1; function add(n) { total += n; }");
    assert_eq!(child.feed_complete("add(41); total"), MontyObject::int(42));
}

#[test]
fn inputs_are_injected() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let mut inputs = NamedValues::new();
    inputs.push("items", MontyObject::list([MontyObject::int(1), MontyObject::int(2)]));
    let (_, event) = child.feed_with("items.map(x => x * 10)", inputs);
    assert_eq!(
        expect_complete(event),
        MontyObject::list([MontyObject::int(10), MontyObject::int(20)])
    );
}

#[test]
fn print_output_is_streamed_in_order() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (prints, event) = child.feed("console.log('a', 1); console.error('b'); console.log({ c: [2] })");
    expect_complete(event);
    let text: String = prints
        .iter()
        .flat_map(|print| print.segments.iter())
        .map(|segment| segment.text.as_str())
        .collect();
    assert_eq!(text, "a 1\nb\n{ c: [ 2 ] }\n");
}

#[test]
fn runtime_error_preserves_session() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    child.feed_complete("const kept = 'still here'");
    let (_, event) = child.feed("null.x");
    let exception = expect_error(event);
    assert_eq!(exception.exc_type, "TypeError");
    assert_eq!(
        exception.message.as_deref(),
        Some("Cannot read properties of null (reading 'x')")
    );
    assert_eq!(child.feed_complete("kept"), MontyObject::string("still here"));
}

#[test]
fn syntax_error_names_the_feed() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) = child.feed("let = ;");
    let exception = expect_error(event);
    assert_eq!(exception.exc_type, "SyntaxError");
    assert_eq!(exception.traceback.len(), 1);
}

#[test]
fn type_checking_is_refused_on_configure() {
    let mut child = ChildProc::spawn();
    child.send(pb::parent_request::Kind::Configure(pb::Configure {
        type_check: true,
        ..configure()
    }));
    let exception = expect_error(child.recv());
    assert_eq!(exception.exc_type, "RuntimeError");
    assert_eq!(
        exception.message.as_deref(),
        Some("type checking is not supported for JavaScript yet")
    );
    // the session was not created, so a feed is a protocol violation and the child survives
    let (_, event) = child.feed("1");
    assert_eq!(expect_error(event).exc_type, "RuntimeError");
    child.create_repl();
    assert_eq!(child.feed_complete("1 + 1"), MontyObject::int(2));
}

#[test]
fn recursion_limit_is_a_catchable_range_error() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let value = child.feed_complete(
        "function f() { return f(); } let m = ''; try { f(); } catch (e) { m = e.constructor.name + ': ' + e.message; } m",
    );
    assert_eq!(
        value,
        MontyObject::string("RangeError: Maximum call stack size exceeded")
    );
}

// =============================================================================
// Host calls
// =============================================================================

#[test]
fn host_function_call_round_trip() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) = child.feed("add(1, 2) * 2");
    let call = expect_function_call(event);
    assert_eq!(call.function_name, "add");
    assert_eq!(call.object_id, None);
    assert_eq!(call_args(&call), vec![MontyObject::int(1), MontyObject::int(2)]);
    let (_, event) = child.resume_call(call.call_id, ExtFunctionResult::Return(MontyObject::int(3)));
    assert_eq!(expect_complete(event), MontyObject::int(6));
}

#[test]
fn host_function_error_is_catchable() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) =
        child.feed("let caught; try { boom(); } catch (e) { caught = e.constructor.name + ': ' + e.message } caught");
    let call = expect_function_call(event);
    assert_eq!(call.function_name, "boom");
    let exc = monty_types::MontyException::new(monty_types::ExcType::ValueError, Some("bad".to_owned()));
    let (_, event) = child.resume_call(call.call_id, ExtFunctionResult::Error(exc));
    assert_eq!(expect_complete(event), MontyObject::string("Error: bad".to_owned()));
}

#[test]
fn host_function_not_found_is_a_reference_error() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) = child.feed("missing()");
    let call = expect_function_call(event);
    let (_, event) = child.resume_call(call.call_id, ExtFunctionResult::NotFound("missing".to_owned()));
    let error = expect_error(event);
    assert_eq!(error.exc_type, "ReferenceError");
    assert_eq!(error.message.as_deref(), Some("missing is not defined"));
    assert_eq!(child.feed_complete("1 + 1"), MontyObject::int(2));
}

#[test]
fn name_lookup_round_trip_and_caches_the_value() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) = child.feed("answer + 1");
    let pb::child_event::Kind::NameLookup(lookup) = event else {
        panic!("expected NameLookup, got {event:?}");
    };
    assert_eq!(lookup.name, "answer");
    let (_, event) = child.resume_name_lookup(NameLookupResult::Value(MontyObject::int(41)));
    assert_eq!(expect_complete(event), MontyObject::int(42));
    // the answer is now a global: no second round-trip
    assert_eq!(child.feed_complete("answer"), MontyObject::int(41));
}

#[test]
fn unknown_name_answers() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) = child.feed("typeof thing");
    assert!(matches!(event, pb::child_event::Kind::NameLookup(_)));
    let (_, event) = child.resume_name_lookup(NameLookupResult::Undefined);
    assert_eq!(expect_complete(event), MontyObject::string("undefined".to_owned()));
    let (_, event) = child.feed("thing");
    assert!(matches!(event, pb::child_event::Kind::NameLookup(_)));
    let (_, event) = child.resume_name_lookup(NameLookupResult::Undefined);
    let error = expect_error(event);
    assert_eq!(error.exc_type, "ReferenceError");
    assert_eq!(error.message.as_deref(), Some("thing is not defined"));
}

#[test]
fn host_future_settles_a_promise() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) = child.feed("const v = await fetchData('k'); v + 1");
    let call = expect_function_call(event);
    assert_eq!(call.function_name, "fetchData");
    let (_, event) = child.resume_call(call.call_id, ExtFunctionResult::Future(call.call_id));
    let pb::child_event::Kind::ResolveFutures(resolve) = event else {
        panic!("expected ResolveFutures, got {event:?}");
    };
    assert_eq!(resolve.pending_call_ids.as_slice(), &[call.call_id]);
    child.send(pb::parent_request::Kind::ResumeFutures(future_results_to_proto(vec![
        (call.call_id, ExtFunctionResult::Return(MontyObject::int(20))),
    ])));
    let (_, event) = child.recv_turn();
    assert_eq!(expect_complete(event), MontyObject::int(21));
}

#[test]
fn host_call_inside_a_builtin_callback_is_an_error() {
    let mut child = ChildProc::spawn();
    child.create_repl();
    let (_, event) = child.feed("[1, 2].map((x) => host(x))");
    let error = expect_error(event);
    assert_eq!(error.exc_type, "TypeError");
    assert_eq!(
        error.message.as_deref(),
        Some("Host function 'host' cannot be called from a callback run by a builtin")
    );
}
