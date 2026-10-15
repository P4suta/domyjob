use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{MAX_CONTROL_BYTES, Phase, Server, catalog, parse, serve_io};
use crate::chat::ops::Session;
use crate::chat::store::Store;

const EXPECTED_TOOL_COUNT: usize = 20;
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
    let server = Server {
        output: Mutex::new(output),
        phase: Mutex::new(Phase::Ready),
        session: Mutex::new(fixture.session.clone_for(Arc::clone(cancelled))),
        client: Mutex::new(None),
        calls: Mutex::new(BTreeMap::new()),
    };
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
            let writer = FaultWriter {
                fault,
                cancelled: Arc::clone(&cancelled),
                bytes: Vec::new(),
            };
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
            let writer = FaultWriter {
                fault: OutputFault::CancelOnFlush,
                cancelled: Arc::clone(&cancelled),
                bytes: Vec::new(),
            };
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
    let (entered, flushing) = std::sync::mpsc::sync_channel(0);
    let (proceed, release) = std::sync::mpsc::sync_channel(0);
    let (server, _prepared) = prepared_read(
        &fixture,
        FlushGate {
            entered,
            proceed: release,
        },
        &cancelled,
        false,
    );
    let request = format!(
        "{}\n",
        call(
            1,
            "chat_read",
            &serde_json::json!({"message":fixture.message.id()}).to_string()
        )
    );
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
    assert_eq!(stage(&fixture), Delivery::Pending);
    assert_eq!(server.calls.lock().unwrap().len(), 0);
}

#[test]
fn substituted_oversized_read_responses_never_mark_read() {
    use crate::chat::ops::reads::tests::{fixture, stage};
    use domyjob_core::chat::notification::Delivery;
    for inbox in [false, true] {
        let fixture = fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (server, mut reply) = prepared_read(&fixture, Vec::new(), &cancelled, inbox);
        reply.value = serde_json::json!({"id":1,"result":"x".repeat(MAX_CONTROL_BYTES)});
        server.deliver_call(reply, &cancelled).unwrap();
        assert_eq!(stage(&fixture), Delivery::Pending);
        assert_eq!(
            field(&replies(server), &1.into(), "/error/code"),
            super::INTERNAL_ERROR
        );
    }
}

#[test]
fn successful_read_delivery_marks_only_the_bound_reader_and_reports_remaining_unread() {
    use crate::chat::ops::reads::tests::{fixture, stage};
    use domyjob_core::chat::notification::Delivery;
    for inbox in [false, true] {
        let fixture = fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (server, reply) = prepared_read(&fixture, Vec::new(), &cancelled, inbox);
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
        server.deliver_call(reply, &cancelled).unwrap();
        assert_eq!(stage(&fixture), Delivery::Read);
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
    }
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
        let tool = tools
            .iter()
            .find(|tool| tool.get("name") == Some(&name.into()))
            .unwrap();
        assert_eq!(
            tool.pointer("/annotations/readOnlyHint"),
            Some(&false.into())
        );
        assert_eq!(
            tool.pointer("/inputSchema/additionalProperties"),
            Some(&false.into())
        );
    }
}
