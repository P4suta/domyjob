use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::Value;

use super::{MAX_CONTROL_BYTES, Phase, Server, catalog, parse, serve_io};
use crate::chat::ops::Session;
use crate::chat::store::Store;

fn server(root: &tempfile::TempDir) -> Server<Vec<u8>> {
    Server {
        output: Mutex::new(Vec::new()),
        phase: Mutex::new(Phase::New),
        session: Mutex::new(Session::with_store(Store::open_in(root.path()).unwrap())),
        client: Mutex::new(None),
        calls: Mutex::new(BTreeMap::new()),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the test decodes the server's own JSON-RPC output"
)]
fn replies(server: Server<Vec<u8>>) -> Vec<Value> {
    let output = server.output.into_inner().unwrap();
    output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

/// The value at a JSON pointer inside the reply with `id`.
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
    assert_eq!(catalog().len(), 18);
}
