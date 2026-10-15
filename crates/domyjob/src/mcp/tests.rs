use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{MAX_CONTROL_BYTES, Phase, Server, catalog, parse, serve_io};
use crate::chat::ops::Session;
use crate::chat::store::Store;

const EXPECTED_TOOL_COUNT: usize = 23;
const INBOX_READ_LIMIT: u32 = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFault {
    Payload,
    Newline,
    Flush,
    CancelOnFlush,
}

struct FaultWriter {
    fault: OutputFault,
    cancelled: Arc<AtomicBool>,
    bytes: Vec<u8>,
}

impl FaultWriter {
    fn new(fault: OutputFault, cancelled: &Arc<AtomicBool>) -> Self {
        Self {
            fault,
            cancelled: Arc::clone(cancelled),
            bytes: Vec::new(),
        }
    }
}

struct FlushGate {
    entered: std::sync::mpsc::SyncSender<()>,
    proceed: std::sync::mpsc::Receiver<()>,
}

impl Write for FlushGate {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.entered.send(()).map_err(io::Error::other)?;
        self.proceed.recv().map_err(io::Error::other)
    }
}

fn cancel_during_flush(request: &str, prepare: impl FnOnce(FlushGate) -> Server<FlushGate>) {
    let (entered, flushing) = std::sync::mpsc::sync_channel(0);
    let (proceed, release) = std::sync::mpsc::sync_channel(0);
    let server = prepare(FlushGate {
        entered,
        proceed: release,
    });
    let registered = std::thread::scope(|scope| {
        let completion = scope.spawn(|| serve_io(request.as_bytes(), &server));
        flushing.recv().unwrap();
        let registered = server.calls.lock().unwrap().contains_key("1");
        super::notification(
            &server,
            "notifications/cancelled",
            &serde_json::json!({"requestId":1}),
        );
        proceed.send(()).unwrap();
        completion.join().unwrap().unwrap();
        registered
    });
    assert!(registered);
    assert_eq!(server.calls.lock().unwrap().len(), 0);
}

impl Write for FaultWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fault == OutputFault::Payload
            || self.fault == OutputFault::Newline && bytes == b"\n"
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "controlled MCP write failure",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fault == OutputFault::Flush {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "controlled MCP flush failure",
            ));
        }
        if self.fault == OutputFault::CancelOnFlush {
            self.cancelled.store(true, Ordering::Release);
        }
        Ok(())
    }
}

fn prepared_read<W: Write + Send>(
    fixture: &crate::chat::ops::reads::tests::Fixture,
    output: W,
    cancelled: &Arc<AtomicBool>,
    inbox: bool,
) -> (Server<W>, super::CallReply) {
    let server = ready_server(fixture.session.clone_for(Arc::clone(cancelled)), output);
    let (tool, args) = if inbox {
        ("chat_inbox", serde_json::json!({"limit":INBOX_READ_LIMIT}))
    } else {
        (
            "chat_read",
            serde_json::json!({"message":fixture.message.id()}),
        )
    };
    let params = serde_json::json!({"name":tool,"arguments":args});
    let reply = server.call(&1.into(), params, cancelled).unwrap();
    assert_eq!(reply.value.pointer("/result/isError"), Some(&false.into()));
    (server, reply)
}

fn for_each_read_reply(
    check: impl Fn(
        &crate::chat::ops::reads::tests::Fixture,
        &Arc<AtomicBool>,
        Server<Vec<u8>>,
        super::CallReply,
    ),
) {
    for inbox in [false, true] {
        let fixture = crate::chat::ops::reads::tests::fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (server, reply) = prepared_read(&fixture, Vec::new(), &cancelled, inbox);
        check(&fixture, &cancelled, server, reply);
    }
}

#[test]
fn read_and_inbox_payload_newline_and_flush_failures_never_mark_read() {
    use crate::chat::ops::reads::tests::{fixture, stage};
    use domyjob_core::chat::notification::Delivery;
    for inbox in [false, true] {
        for fault in [
            OutputFault::Payload,
            OutputFault::Newline,
            OutputFault::Flush,
        ] {
            let fixture = fixture();
            let cancelled = Arc::new(AtomicBool::new(false));
            let writer = FaultWriter::new(fault, &cancelled);
            let (server, reply) = prepared_read(&fixture, writer, &cancelled, inbox);
            assert_eq!(stage(&fixture), Delivery::Pending);
            let error = server.deliver_call(reply, &cancelled).unwrap_err();
            assert!(matches!(error, super::McpError::Io(_)));
            assert_eq!(stage(&fixture), Delivery::Pending);
        }
    }
}

#[test]
fn cancellation_before_or_during_read_output_never_marks_read() {
    use crate::chat::ops::reads::tests::{fixture, stage};
    use domyjob_core::chat::notification::Delivery;
    for inbox in [false, true] {
        for before in [false, true] {
            let fixture = fixture();
            let cancelled = Arc::new(AtomicBool::new(false));
            let writer = FaultWriter::new(OutputFault::CancelOnFlush, &cancelled);
            let (server, reply) = prepared_read(&fixture, writer, &cancelled, inbox);
            if before {
                cancelled.store(true, Ordering::Release);
            }
            server.deliver_call(reply, &cancelled).unwrap();
            assert_eq!(stage(&fixture), Delivery::Pending);
            assert_eq!(server.output.lock().unwrap().bytes.is_empty(), before);
        }
    }
}

#[test]
fn read_cancellation_stays_registered_until_response_flush_completes() {
    use crate::chat::ops::reads::tests::{fixture, stage};
    use domyjob_core::chat::notification::Delivery;
    let fixture = fixture();
    let cancelled = Arc::new(AtomicBool::new(false));
    let request = format!(
        "{}\n",
        call(
            1,
            "chat_read",
            &serde_json::json!({"message":fixture.message.id()}).to_string()
        )
    );
    cancel_during_flush(&request, |output| {
        prepared_read(&fixture, output, &cancelled, false).0
    });
    assert_eq!(stage(&fixture), Delivery::Pending);
}

#[test]
fn substituted_oversized_read_responses_never_mark_read() {
    use crate::chat::ops::reads::tests::stage;
    use domyjob_core::chat::notification::Delivery;
    for_each_read_reply(|fixture, cancelled, server, mut reply| {
        reply.value = serde_json::json!({"id":1,"result":"x".repeat(MAX_CONTROL_BYTES)});
        server.deliver_call(reply, cancelled).unwrap();
        assert_eq!(stage(fixture), Delivery::Pending);
        assert_eq!(
            field(&replies(server), &1.into(), "/error/code"),
            super::INTERNAL_ERROR
        );
    });
}

#[test]
fn successful_read_delivery_marks_only_the_bound_reader_and_reports_remaining_unread() {
    use crate::chat::ops::reads::tests::stage;
    use domyjob_core::chat::notification::Delivery;
    for_each_read_reply(|fixture, cancelled, server, reply| {
        assert_eq!(
            reply.value.pointer("/result/structuredContent/unread"),
            Some(&0.into())
        );
        server
            .session
            .lock()
            .unwrap()
            .bind(fixture.sender.name().as_str())
            .unwrap();
        server.deliver_call(reply, cancelled).unwrap();
        assert_eq!(stage(fixture), Delivery::Read);
        assert_eq!(
            fixture
                .session
                .store
                .read(|read| crate::chat::store::delivery(
                    read,
                    &fixture.sender,
                    fixture.message.id()
                ))
                .unwrap(),
            Delivery::Pending
        );
    });
}

fn server(root: &tempfile::TempDir) -> Server<Vec<u8>> {
    Server {
        output: Mutex::new(Vec::new()),
        phase: Mutex::new(Phase::New),
        session: Mutex::new(Session::with_store(
            Store::open_in(&crate::layout::State::at(root.path())).unwrap(),
        )),
        client: Mutex::new(None),
        calls: Mutex::new(BTreeMap::new()),
    }
}

fn replies(server: Server<Vec<u8>>) -> Vec<Value> {
    let output = server.output.into_inner().unwrap();
    output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| domyjob_core::ingress::json(line, usize::MAX).unwrap())
        .collect()
}

fn field<'a>(replies: &'a [Value], id: &Value, pointer: &str) -> &'a Value {
    replies
        .iter()
        .find(|reply| reply.get("id") == Some(id))
        .and_then(|reply| reply.pointer(pointer))
        .unwrap_or(&Value::Null)
}

fn lines(requests: &[&str]) -> String {
    let mut text = concat!(
        r#"{"jsonrpc":"2.0","id":"start","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"claude-code","version":"1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n"
    )
    .to_owned();
    for request in requests {
        text.push_str(request);
        text.push('\n');
    }
    text
}

fn call(id: u64, name: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
    )
}

#[test]
fn initialization_gates_tools_and_negotiates_a_supported_version() {
    let root = tempfile::tempdir().unwrap();
    let server = server(&root);
    let early = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let mut input = format!("{early}\n");
    input.push_str(&lines(&[
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"unknown"}"#,
    ]));
    serve_io(input.as_bytes(), &server).unwrap();
    let replies = replies(server);
    assert_eq!(field(&replies, &1.into(), "/error/code"), -32002);
    assert_eq!(
        field(&replies, &"start".into(), "/result/protocolVersion"),
        "2025-03-26"
    );
    let tools = field(&replies, &2.into(), "/result/tools")
        .as_array()
        .unwrap();
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name") == Some(&"chat_directory".into())
                && tool.pointer("/annotations/readOnlyHint") == Some(&Value::Bool(true)))
    );
    assert_eq!(field(&replies, &3.into(), "/error/code"), -32601);
}

#[test]
fn writes_require_an_identity_and_reads_work_without_one() {
    let root = tempfile::tempdir().unwrap();
    let server = server(&root);
    let input = lines(&[
        &call(7, "chat_send", r#"{"target":"x","text":"hi"}"#),
        &call(8, "chat_rooms", "{}"),
        &call(9, "chat_read", r#"{"message":"unbound"}"#),
        &call(10, "chat_acknowledge", r#"{"message":"unbound"}"#),
    ]);
    serve_io(input.as_bytes(), &server).unwrap();
    let replies = replies(server);
    assert_eq!(field(&replies, &7.into(), "/result/isError"), true);
    assert!(
        field(&replies, &7.into(), "/result/content/0/text")
            .as_str()
            .unwrap()
            .contains("identity")
    );
    assert_eq!(field(&replies, &8.into(), "/result/isError"), false);
    for id in [9, 10] {
        assert_eq!(field(&replies, &id.into(), "/result/isError"), true);
        assert!(
            field(&replies, &id.into(), "/result/content/0/text")
                .as_str()
                .unwrap()
                .contains("identity")
        );
    }
}

#[test]
fn joining_binds_the_session_and_later_results_report_unread_messages() {
    let root = tempfile::tempdir().unwrap();
    let server = server(&root);
    let input = lines(&[&call(
        1,
        "chat_join",
        r#"{"name":"assistant","profile":{"role":"helper","skills":["rust"]}}"#,
    )]);
    serve_io(input.as_bytes(), &server).unwrap();
    let later = format!("{}\n", call(2, "chat_whoami", "{}"));
    serve_io(later.as_bytes(), &server).unwrap();
    let replies = replies(server);
    assert_eq!(field(&replies, &1.into(), "/result/isError"), false);
    assert_eq!(
        field(&replies, &1.into(), "/result/structuredContent/card/tool"),
        "claude"
    );
    assert_eq!(
        field(&replies, &2.into(), "/result/structuredContent/card/role"),
        "helper"
    );
    assert_eq!(
        field(&replies, &2.into(), "/result/structuredContent/unread"),
        0
    );
}

#[test]
fn malformed_and_oversized_input_is_bounded() {
    for bytes in [
        b"[]".as_slice(),
        br#"{"jsonrpc":"2.0","method":"ping","id":true}"#,
        b"{",
        br#"{"jsonrpc":"1.0","method":"ping","id":1}"#,
    ] {
        parse(bytes).unwrap_err();
    }
    let root = tempfile::tempdir().unwrap();
    let server = server(&root);
    let input = vec![b' '; MAX_CONTROL_BYTES.saturating_add(100)];
    serve_io(input.as_slice(), &server).unwrap_err();
    assert!(server.output.lock().unwrap().len() < 256);
    let tools = catalog();
    assert_eq!(tools.len(), EXPECTED_TOOL_COUNT);
    for name in ["chat_read", "chat_acknowledge"] {
        assert_tool_contract(name, false);
    }
}

fn assert_tool_contract(name: &str, read_only: bool) -> Value {
    let tool = catalog()
        .into_iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        .unwrap();
    assert_eq!(
        tool.pointer("/annotations/readOnlyHint"),
        Some(&read_only.into())
    );
    assert_eq!(
        tool.pointer("/inputSchema/additionalProperties"),
        Some(&false.into())
    );
    tool
}

fn prepared_handoff<W: Write + Send>(
    fixture: &crate::chat::ops::handoff::tests::Fixture,
    output: W,
    cancelled: &Arc<AtomicBool>,
) -> (Server<W>, super::CallReply) {
    let server = handoff_server(fixture, output, cancelled);
    let request = parse(handoff_request(&fixture.descriptor).as_bytes()).unwrap();
    let reply = server.call(&1.into(), request.params, cancelled).unwrap();
    assert_eq!(reply.value.pointer("/result/isError"), Some(&false.into()));
    assert!(!handoff_was_observed(fixture));
    (server, reply)
}

fn handoff_server<W>(
    fixture: &crate::chat::ops::handoff::tests::Fixture,
    output: W,
    cancelled: &Arc<AtomicBool>,
) -> Server<W> {
    ready_server(fixture.continuing.clone_for(Arc::clone(cancelled)), output)
}

fn ready_server<W>(session: Session, output: W) -> Server<W> {
    Server {
        output: Mutex::new(output),
        phase: Mutex::new(Phase::Ready),
        session: Mutex::new(session),
        client: Mutex::new(None),
        calls: Mutex::new(BTreeMap::new()),
    }
}

fn handoff_request(descriptor: &domyjob_core::chat::handoff::Descriptor) -> String {
    call(
        1,
        "chat_handoff_read",
        &serde_json::json!({"descriptor": descriptor, "max_bytes": descriptor.body_bytes()})
            .to_string(),
    )
}

fn handoff_was_observed(fixture: &crate::chat::ops::handoff::tests::Fixture) -> bool {
    fixture.observed()
}

#[test]
fn scoped_handoffs_have_dedicated_typed_tools_and_honest_effects() {
    for (name, read_only) in [
        ("chat_handoff_create", false),
        ("chat_handoff_list", true),
        ("chat_handoff_read", false),
    ] {
        let tool = assert_tool_contract(name, read_only);
        assert!(
            tool.pointer("/inputSchema/properties")
                .unwrap()
                .get("actor")
                .is_none()
        );
    }
}

#[test]
fn a_delivered_handoff_records_only_the_bound_continuing_reader() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let server = handoff_server(&fixture, Vec::new(), &Arc::new(AtomicBool::new(false)));
    let request = format!("{}\n", handoff_request(&fixture.descriptor));
    serve_io(request.as_bytes(), &server).unwrap();
    let responses = replies(server);
    assert_eq!(field(&responses, &1.into(), "/result/isError"), false);
    assert_eq!(
        field(&responses, &1.into(), "/result/structuredContent/reader"),
        &serde_json::json!(fixture.continuing.me().unwrap())
    );
    assert!(handoff_was_observed(&fixture));
}

#[test]
fn failed_handoff_payload_newline_and_flush_preserve_the_io_cause_without_observation() {
    for fault in [
        OutputFault::Payload,
        OutputFault::Newline,
        OutputFault::Flush,
    ] {
        let fixture = crate::chat::ops::handoff::tests::fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let output = FaultWriter::new(fault, &cancelled);
        let (server, reply) = prepared_handoff(&fixture, output, &cancelled);
        let error = server.deliver_call(reply, &cancelled).unwrap_err();
        let (kind, message) = if fault == OutputFault::Flush {
            (io::ErrorKind::StorageFull, "controlled MCP flush failure")
        } else {
            (io::ErrorKind::BrokenPipe, "controlled MCP write failure")
        };
        assert!(
            matches!(error, super::McpError::Io(cause) if cause.kind() == kind && cause.to_string() == message)
        );
        assert!(!handoff_was_observed(&fixture));
    }
}

#[test]
fn cancelled_handoff_delivery_never_records_an_observation() {
    for during_flush in [false, true] {
        let fixture = crate::chat::ops::handoff::tests::fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let output = FaultWriter::new(OutputFault::CancelOnFlush, &cancelled);
        let (server, reply) = prepared_handoff(&fixture, output, &cancelled);
        if !during_flush {
            cancelled.store(true, Ordering::Release);
        }
        server.deliver_call(reply, &cancelled).unwrap();
        assert!(!handoff_was_observed(&fixture));
        assert_eq!(
            server.output.lock().unwrap().bytes.is_empty(),
            !during_flush
        );
    }
}

#[test]
fn cancellation_remains_registered_until_the_actual_response_flush_completes() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let request = format!("{}\n", handoff_request(&fixture.descriptor));
    cancel_during_flush(&request, |output| {
        handoff_server(&fixture, output, &Arc::new(AtomicBool::new(false)))
    });
    assert!(!handoff_was_observed(&fixture));
}

#[test]
fn a_replaced_oversized_reply_never_records_the_undelivered_handoff() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let cancelled = Arc::new(AtomicBool::new(false));
    let (server, mut reply) = prepared_handoff(&fixture, Vec::new(), &cancelled);
    reply.value = serde_json::json!({"id":1,"result":"x".repeat(MAX_CONTROL_BYTES)});
    server.deliver_call(reply, &cancelled).unwrap();
    assert!(!handoff_was_observed(&fixture));
    let responses = replies(server);
    assert_eq!(
        field(&responses, &1.into(), "/error/code"),
        super::INTERNAL_ERROR
    );
}

#[test]
fn a_later_session_identity_cannot_replace_the_reader_bound_to_the_delivered_body() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let cancelled = Arc::new(AtomicBool::new(false));
    let (server, reply) = prepared_handoff(&fixture, Vec::new(), &cancelled);
    let stranger = crate::chat::store::fixtures::register(
        &fixture.continuing.store,
        "ungranted",
        domyjob_core::chat::card::Mode::Interactive,
    );
    server.session.lock().unwrap().bind("ungranted").unwrap();
    server.deliver_call(reply, &cancelled).unwrap();
    assert!(handoff_was_observed(&fixture));
    assert!(
        !fixture
            .continuing
            .store
            .read(|read| crate::chat::store::handoff::observed(
                read,
                &stranger,
                fixture.descriptor.id()
            ))
            .unwrap()
    );
}

#[test]
fn handoff_dispatch_preserves_own_identity_and_discovery_excludes_body_text() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let mut sender = fixture.sender.clone_for(Arc::new(AtomicBool::new(false)));
    let created = super::dispatch(&mut sender, "chat_handoff_create", serde_json::json!({
        "project":"owner/project", "scope":"new-scope", "intended_recipient":{"kind":"agent","agent":fixture.continuing.me().unwrap()},
        "permitted_readers":[fixture.continuing.me().unwrap()], "text":"new scoped body"
    }), None).unwrap();
    assert_eq!(
        created.json().pointer("/handoff/metadata/sender"),
        Some(&serde_json::json!(sender.me().unwrap()))
    );
    let mut continuing = fixture
        .continuing
        .clone_for(Arc::new(AtomicBool::new(false)));
    let listed = super::dispatch(
        &mut continuing,
        "chat_handoff_list",
        serde_json::json!({"project":"owner/project","scope":"issue-47","limit":1}),
        None,
    )
    .unwrap();
    let value = listed.json();
    assert_eq!(
        value.get("handoffs"),
        Some(&serde_json::json!([fixture.descriptor]))
    );
    assert!(value.get("text").is_none());
    assert!(!handoff_was_observed(&fixture));
}

#[test]
fn a_fresh_ungranted_identity_cannot_read_or_impersonate_a_granted_reader() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let mut stranger = Session::with_store(fixture.continuing.store.clone());
    super::dispatch(
        &mut stranger,
        "chat_join",
        serde_json::json!({"name":"previous"}),
        Some(domyjob_core::chat::card::Tool::Codex),
    )
    .unwrap();
    let domyjob_core::chat::handoff::Target::Agent { agent: previous } =
        fixture.descriptor.metadata().intended_recipient()
    else {
        panic!("named previous reader fixture")
    };
    assert_eq!(stranger.me().unwrap().name(), previous.name());
    assert_ne!(stranger.me().unwrap(), previous);
    let request = parse(handoff_request(&fixture.descriptor).as_bytes()).unwrap();
    let mut arguments = request.params.get("arguments").unwrap().clone();
    let error =
        super::dispatch(&mut stranger, "chat_handoff_read", arguments.clone(), None).unwrap_err();
    assert!(matches!(
        error,
        super::CallError::Ops(crate::chat::ops::OpsError::Store(
            crate::chat::store::StoreError::Handoff(
                domyjob_core::chat::handoff::ReadRefusal::Reader
            )
        ))
    ));
    arguments.as_object_mut().unwrap().insert(
        "actor".to_owned(),
        serde_json::json!(fixture.continuing.me().unwrap()),
    );
    assert!(matches!(
        super::dispatch(&mut stranger, "chat_handoff_read", arguments, None),
        Err(super::CallError::Arguments(_))
    ));
    assert!(!handoff_was_observed(&fixture));
}

#[test]
fn handoff_schemas_publish_context_body_grant_and_read_limits() {
    let tools = catalog();
    let schema = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.get("name") == Some(&name.into()))
            .unwrap()
            .get("inputSchema")
            .unwrap()
    };
    let create = schema("chat_handoff_create");
    assert_eq!(
        create.pointer("/properties/project/maxLength"),
        Some(&serde_json::json!(
            domyjob_core::chat::handoff::MAX_CONTEXT_BYTES
        ))
    );
    assert_eq!(
        create.pointer("/properties/scope/minLength"),
        Some(&1.into())
    );
    assert_eq!(
        create.pointer("/properties/text/maxLength"),
        Some(&serde_json::json!(
            domyjob_core::chat::handoff::MAX_BODY_BYTES
        ))
    );
    assert_eq!(
        create.pointer("/properties/permitted_readers/maxItems"),
        Some(&serde_json::json!(domyjob_core::chat::event::MAX_MEMBERS))
    );
    assert_eq!(
        create.pointer("/properties/permitted_readers/uniqueItems"),
        Some(&true.into())
    );
    let read = schema("chat_handoff_read");
    assert_eq!(
        read.pointer("/properties/max_bytes/minimum"),
        Some(&1.into())
    );
    assert_eq!(
        read.pointer("/properties/max_bytes/maximum"),
        Some(&serde_json::json!(
            domyjob_core::chat::handoff::MAX_BODY_BYTES
        ))
    );
    let list = schema("chat_handoff_list");
    assert_eq!(list.pointer("/properties/limit/minimum"), Some(&1.into()));
    assert_eq!(
        list.pointer("/properties/limit/maximum"),
        Some(&serde_json::json!(
            crate::chat::store::handoff::MAX_HANDOFFS
        ))
    );
}

#[test]
fn invalid_handoff_arguments_fail_before_any_read_observation() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let mut continuing = fixture
        .continuing
        .clone_for(Arc::new(AtomicBool::new(false)));
    let before = continuing.store.local_seen().unwrap();
    for max_bytes in [0, domyjob_core::chat::handoff::MAX_BODY_BYTES + 1] {
        let result = super::dispatch(
            &mut continuing,
            "chat_handoff_read",
            serde_json::json!({"descriptor":fixture.descriptor,"max_bytes":max_bytes}),
            None,
        );
        assert!(matches!(result, Err(super::CallError::Arguments(_))));
    }
    let result = super::dispatch(
        &mut continuing,
        "chat_handoff_list",
        serde_json::json!({"project":"x".repeat(domyjob_core::chat::handoff::MAX_CONTEXT_BYTES + 1),"scope":"issue-47"}),
        None,
    );
    assert!(matches!(result, Err(super::CallError::Arguments(_))));
    assert_eq!(continuing.store.local_seen().unwrap(), before);
    assert!(!handoff_was_observed(&fixture));
}

#[test]
fn scoped_handoff_formatting_cannot_reach_the_unrelated_inbox_port() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let list = domyjob_core::ingress::value(
        serde_json::json!({"project":"owner/project","scope":"issue-47"}),
    )
    .unwrap();
    let outcomes = [
        crate::chat::view::Outcome::HandoffCreated(fixture.descriptor.clone()),
        crate::chat::ops::handoffs(&fixture.continuing, &list).unwrap(),
        crate::chat::ops::read_handoff(
            &fixture.continuing,
            &crate::chat::ops::handoff::tests::read_args(&fixture.descriptor),
        )
        .unwrap(),
    ];
    for outcome in outcomes {
        let attempted = std::cell::Cell::new(0);
        let value = super::rendered_outcome_using(&outcome, || {
            attempted.set(attempted.get() + 1);
            Err(crate::chat::ops::OpsError::Usage(
                "unrelated inbox attempted",
            ))
        });
        assert_eq!(attempted.get(), 0);
        assert!(value.get("unread").is_none());
    }
    assert!(!handoff_was_observed(&fixture));
}

#[test]
fn ordinary_chat_formatting_preserves_the_existing_unread_port() {
    let fixture = crate::chat::ops::handoff::tests::fixture();
    let outcome = crate::chat::ops::rooms(&fixture.continuing).unwrap();
    let attempted = std::cell::Cell::new(0);
    let value = super::rendered_outcome_using(&outcome, || {
        attempted.set(attempted.get() + 1);
        Ok(Some(3))
    });
    assert_eq!(attempted.get(), 1);
    assert_eq!(value.get("unread"), Some(&3.into()));
}
