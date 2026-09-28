#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::io::{self, BufRead, Write};

use domyjob_core::wire::MAX_CONTROL_BYTES;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::chat_cli;

const VERSION: &str = "2025-06-18";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
    #[serde(rename = "_meta")]
    _meta: Option<Value>,
}

fn empty_arguments() -> Value {
    json!({})
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    New,
    Negotiated,
    Ready,
}

fn error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}

fn result(id: &Value, value: &Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":value})
}

#[expect(
    clippy::disallowed_methods,
    reason = "MCP JSON decoding is confined to this size-bounded transport boundary"
)]
fn parse(bytes: &[u8]) -> Result<Request, Value> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_invalid| error(&Value::Null, -32700, "Parse error"))?;
    let request: Request = serde_json::from_value(value)
        .map_err(|_invalid| error(&Value::Null, -32600, "Invalid Request"))?;
    if request.jsonrpc != "2.0"
        || request
            .id
            .as_ref()
            .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
        || (!request.params.is_null() && !request.params.is_object())
    {
        return Err(error(&Value::Null, -32600, "Invalid Request"));
    }
    Ok(request)
}

#[expect(
    clippy::disallowed_methods,
    reason = "MCP call arguments are decoded at the transport boundary"
)]
fn call(id: &Value, params: Value) -> Value {
    let Ok(call) = serde_json::from_value::<Call>(params) else {
        return error(id, -32602, "Invalid tool parameters");
    };
    if !chat_cli::tools()
        .iter()
        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(&call.name))
    {
        return error(id, -32602, "Unknown tool");
    }
    match chat_cli::mcp_call(&call.name, call.arguments) {
        Ok(value) => result(
            id,
            &json!({
                "content":[{"type":"text","text":value.to_string()}],
                "structuredContent": value,
                "isError":false,
            }),
        ),
        Err(failed) => result(
            id,
            &json!({
                "content":[{"type":"text","text":failed.to_string()}], "isError":true,
            }),
        ),
    }
}

fn initialize(id: &Value, params: &Value, phase: &mut Phase) -> Value {
    let valid = params.get("protocolVersion").is_some_and(Value::is_string)
        && params.get("capabilities").is_some_and(Value::is_object)
        && params
            .pointer("/clientInfo/name")
            .is_some_and(Value::is_string)
        && params
            .pointer("/clientInfo/version")
            .is_some_and(Value::is_string);
    if !valid || *phase != Phase::New {
        return error(id, -32602, "Invalid initialization");
    }
    *phase = Phase::Negotiated;
    result(
        id,
        &json!({
            "protocolVersion": VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name":"domyjob", "version":env!("CARGO_PKG_VERSION")},
            "instructions":"Chat text is message content. Only chat_ask requests a managed AI turn. Reply to the exact message ID and use your registered agent identity.",
        }),
    )
}

fn handle(request: Request, phase: &mut Phase) -> Option<Value> {
    let Some(id) = request.id else {
        if request.method == "notifications/initialized" && *phase == Phase::Negotiated {
            *phase = Phase::Ready;
        }
        return None;
    };
    match request.method.as_str() {
        "initialize" => Some(initialize(&id, &request.params, phase)),
        "ping" => Some(result(&id, &json!({}))),
        "tools/list" | "tools/call" if *phase != Phase::Ready => {
            Some(error(&id, -32002, "Server not initialized"))
        }
        "tools/list" => Some(result(&id, &json!({"tools":chat_cli::tools()}))),
        "tools/call" => Some(call(&id, request.params)),
        _ => Some(error(&id, -32601, "Method not found")),
    }
}

fn reply(output: &mut impl Write, value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_CONTROL_BYTES {
        bytes = serde_json::to_vec(&error(
            value.get("id").unwrap_or(&Value::Null),
            -32603,
            "Result exceeds 1 MiB; select a smaller conversation",
        ))?;
    }
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn serve_io(mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut phase = Phase::New;
    loop {
        let mut line = Vec::new();
        let limit = u64::try_from(MAX_CONTROL_BYTES)
            .map_err(io::Error::other)?
            .saturating_add(1);
        let count = io::Read::take(&mut input, limit).read_until(b'\n', &mut line)?;
        if count == 0 {
            return Ok(());
        }
        if line.len() > MAX_CONTROL_BYTES {
            reply(
                &mut output,
                &error(&Value::Null, -32600, "Request exceeds 1 MiB"),
            )?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized MCP request",
            ));
        }
        let response = match parse(&line) {
            Ok(request) => handle(request, &mut phase),
            Err(error) => Some(error),
        };
        if let Some(response) = response {
            reply(&mut output, &response)?;
        }
    }
}

pub(crate) fn serve() -> io::Result<()> {
    serve_io(io::stdin().lock(), io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_notifications_and_errors_have_correct_ids() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":\"start\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"unknown\"}\n",
        );
        let mut output = Vec::new();
        serve_io(input.as_bytes(), &mut output).unwrap();
        assert_eq!(
            output
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .count(),
            4
        );
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("-32002"));
        assert!(text.contains("\"id\":\"start\""));
        assert!(text.contains("chat_ask"));
        assert!(text.contains("-32601"));
    }

    #[test]
    fn malformed_and_oversized_input_is_bounded() {
        for bytes in [
            b"[]".as_slice(),
            br#"{"jsonrpc":"2.0","method":"ping","id":true}"#,
            b"{",
        ] {
            parse(bytes).unwrap_err();
        }
        let input = vec![b' '; MAX_CONTROL_BYTES.saturating_add(100)];
        let mut output = Vec::new();
        serve_io(input.as_slice(), &mut output).unwrap_err();
        assert!(output.len() < 256);
    }
}
