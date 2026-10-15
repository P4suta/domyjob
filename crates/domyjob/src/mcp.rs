use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use domyjob_core::chat::card::Tool;
use domyjob_core::chat::handoff::{DeliveryOutcome, observation_after_delivery};
use domyjob_core::chat::notification::{ResponseDelivery, reading_after_delivery};
use domyjob_core::ingress;

use crate::output::Output;
use domyjob_core::wire::MAX_CONTROL_BYTES;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::chat::args::{
    AskArgs, DirectoryArgs, EmptyArgs, HandoffCreateArgs, HandoffListArgs, HandoffReadArgs,
    InboxArgs, JoinArgs, MemberArgs, MessageArgs, NameArgs, ProfileArgs, ReplyArgs, RoomArgs,
    SendArgs, StartArgs, ThreadArgs, TopicArgs,
};
use crate::chat::ops::{self, OpsError, RoomChange, Session};
use crate::chat::store::handoff::PendingObservation;
use crate::chat::view::Outcome;

const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_CALLS: usize = 32;
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const SERVER_NOT_INITIALIZED: i32 = -32002;
const REQUEST_REFUSED: i32 = -32000;
const INSTRUCTIONS: &str = "Chat with AI agents on this and other machines. \
Call chat_whoami, or chat_join to register this session under a name before sending. \
Use chat_directory to find the agent whose role and skills fit a task, then chat_ask it; \
chat_ask waits for the answer and chat_wait resumes waiting. \
Replies go to exact message IDs. Results report `unread`; read them with chat_inbox. \
For scoped continuation, use chat_handoff_list and chat_handoff_read under your own chat_join identity.";

#[derive(Debug, thiserror::Error)]
pub(crate) enum McpError {
    #[error(transparent)]
    Ops(#[from] OpsError),
    #[error("MCP input or output failed: {0}")]
    Io(#[from] io::Error),
}

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
    #[serde(rename = "_meta", default)]
    _meta: Option<Value>,
}

fn empty_arguments() -> Value {
    json!({})
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
    ingress::value(value)
}

fn parse(bytes: &[u8]) -> Result<Request, Value> {
    let value: Value = ingress::json(bytes, MAX_CONTROL_BYTES)
        .map_err(|_invalid| error(&Value::Null, PARSE_ERROR, "Parse error"))?;
    let request: Request = decode(value)
        .map_err(|_invalid| error(&Value::Null, INVALID_REQUEST, "Invalid Request"))?;
    if request.jsonrpc != "2.0"
        || request
            .id
            .as_ref()
            .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
        || (!request.params.is_null() && !request.params.is_object())
    {
        return Err(error(&Value::Null, INVALID_REQUEST, "Invalid Request"));
    }
    Ok(request)
}

fn error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn result(id: &Value, value: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": value})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Read,
    Write,
    Destructive,
}

fn annotations(title: &str, effect: Effect) -> Value {
    json!({
        "title": title,
        "readOnlyHint": effect == Effect::Read,
        "destructiveHint": effect == Effect::Destructive,
        "idempotentHint": effect == Effect::Read,
        "openWorldHint": false,
    })
}

type Run<A> = fn(&mut Session, &A, Option<Tool>) -> Result<Outcome, OpsError>;

macro_rules! tools {
    ($(($name:literal, $title:literal, $description:literal, $args:ty, $effect:expr, $run:expr)),+ $(,)?) => {
        fn catalog() -> Vec<Value> {
            vec![$(json!({
                "name": $name,
                "title": $title,
                "description": $description,
                "inputSchema": schemars::schema_for!($args),
                "annotations": annotations($title, $effect),
            })),+]
        }

        fn reads(name: &str) -> bool {
            match name {
                $($name => $effect == Effect::Read,)+
                _ => false,
            }
        }

        fn dispatch(session: &mut Session, name: &str, input: Value, client: Option<Tool>) -> Result<Outcome, CallError> {
            match name {
                $($name => {
                    let arguments: $args = decode(input).map_err(CallError::Arguments)?;
                    let run: Run<$args> = $run;
                    let read = reads(name);
                    ops::around(session, read, |session| run(session, &arguments, client)).map_err(CallError::Ops)
                })+
                _ => Err(CallError::Unknown),
            }
        }
    };
}

#[derive(Debug)]
enum CallError {
    Unknown,
    Arguments(serde_json::Error),
    Ops(OpsError),
}

tools!(
    (
        "chat_join",
        "Join",
        "Register this session as an agent with a profile and act as it.",
        JoinArgs,
        Effect::Write,
        |session, args, client| {
            let tool = args
                .tool
                .map(Tool::from)
                .or(client)
                .ok_or(OpsError::Usage("this client is unknown; pass tool"))?;
            ops::join(session, args, tool)
        }
    ),
    (
        "chat_whoami",
        "Who am I",
        "Show the agent this session acts as, with its profile and unread count.",
        EmptyArgs,
        Effect::Read,
        |session, _args, _client| ops::whoami(session)
    ),
    (
        "chat_directory",
        "Directory",
        "Find agents and rooms by name, role, skill, project, or description.",
        DirectoryArgs,
        Effect::Read,
        |session, args, _client| ops::directory(session, args)
    ),
    (
        "chat_profile",
        "Update profile",
        "Change this agent's display name, role, description, skills, project, or status.",
        ProfileArgs,
        Effect::Write,
        |session, args, _client| ops::profile(session, args)
    ),
    (
        "chat_agent_start",
        "Start agent",
        "Register a managed agent whose turns this machine runs with an AI CLI.",
        StartArgs,
        Effect::Write,
        |session, args, _client| ops::start(session, args)
    ),
    (
        "chat_agent_remove",
        "Remove agent",
        "Remove a local agent and end the asks waiting on it.",
        NameArgs,
        Effect::Destructive,
        |session, args, _client| ops::remove(session, args)
    ),
    (
        "chat_rooms",
        "Rooms",
        "List rooms with their topics and members.",
        EmptyArgs,
        Effect::Read,
        |session, _args, _client| ops::rooms(session)
    ),
    (
        "chat_room_create",
        "Create room",
        "Create a room owned by this machine with you and the given agents.",
        RoomArgs,
        Effect::Write,
        |session, args, _client| ops::create_room(session, args)
    ),
    (
        "chat_room_add",
        "Add member",
        "Add an agent to a room owned by this machine.",
        MemberArgs,
        Effect::Write,
        |session, args, _client| ops::change_room(session, args, RoomChange::Add)
    ),
    (
        "chat_room_remove",
        "Remove member",
        "Remove an agent from a room owned by this machine.",
        MemberArgs,
        Effect::Write,
        |session, args, _client| ops::change_room(session, args, RoomChange::Remove)
    ),
    (
        "chat_room_topic",
        "Set topic",
        "Set or clear a room's topic.",
        TopicArgs,
        Effect::Write,
        |session, args, _client| ops::set_topic(session, args)
    ),
    (
        "chat_send",
        "Send",
        "Send a message to an agent or a room.",
        SendArgs,
        Effect::Write,
        |session, args, _client| ops::send(session, args)
    ),
    (
        "chat_ask",
        "Ask",
        "Ask one agent and wait for its answer or ending.",
        AskArgs,
        Effect::Write,
        |session, args, _client| ops::ask(session, args)
    ),
    (
        "chat_reply",
        "Reply",
        "Reply to an exact message ID.",
        ReplyArgs,
        Effect::Write,
        |session, args, _client| ops::reply(session, args)
    ),
    (
        "chat_withdraw",
        "Withdraw",
        "Withdraw an ask you sent.",
        MessageArgs,
        Effect::Write,
        |session, args, _client| ops::withdraw(session, args)
    ),
    (
        "chat_wait",
        "Wait",
        "Show an ask's state, waiting up to `timeout` seconds for its ending.",
        MessageArgs,
        Effect::Read,
        |session, args, _client| ops::wait(session, args)
    ),
    (
        "chat_inbox",
        "Inbox",
        "Read the messages addressed to you that you have not read.",
        InboxArgs,
        Effect::Write,
        |session, args, _client| ops::inbox(session, args)
    ),
    (
        "chat_thread",
        "Thread",
        "Read a conversation with an agent or a room.",
        ThreadArgs,
        Effect::Read,
        |session, args, _client| ops::thread(session, args)
    ),
    (
        "chat_read",
        "Read message",
        "Read the full body of an addressed message and record it as read.",
        MessageArgs,
        Effect::Write,
        |session, args, _client| ops::read_message(session, args)
    ),
    (
        "chat_handoff_create",
        "Create handoff",
        "Create a project-scoped handoff under your identity with explicit named reader grants.",
        HandoffCreateArgs,
        Effect::Write,
        |session, args, _client| ops::create_handoff(session, args)
    ),
    (
        "chat_handoff_list",
        "List handoffs",
        "Discover bounded handoff metadata granted to your identity in one exact project and scope.",
        HandoffListArgs,
        Effect::Read,
        |session, args, _client| ops::handoffs(session, args)
    ),
    (
        "chat_handoff_read",
        "Read handoff",
        "Read one granted handoff using its exact descriptor and record your observation after delivery.",
        HandoffReadArgs,
        Effect::Write,
        |session, args, _client| ops::read_handoff(session, args)
    ),
    (
        "chat_acknowledge",
        "Acknowledge",
        "Acknowledge an addressed message and release its urgent stop block.",
        MessageArgs,
        Effect::Write,
        |session, args, _client| ops::acknowledge(session, args)
    ),
);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    New,
    Negotiated,
    Ready,
}

struct Server<W> {
    output: Mutex<W>,
    phase: Mutex<Phase>,
    session: Mutex<Session>,
    client: Mutex<Option<Tool>>,
    calls: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
}

enum PendingDelivery {
    Messages(Box<ops::PendingRead>),
    Handoff(Box<PendingObservation>),
}

impl PendingDelivery {
    const fn admits(&self, delivered: ResponseDelivery, cancelled: bool) -> bool {
        match self {
            Self::Messages(_) => reading_after_delivery(delivered, cancelled),
            Self::Handoff(_) => observation_after_delivery(
                match delivered {
                    ResponseDelivery::Original => DeliveryOutcome::OriginalBody,
                    ResponseDelivery::Replaced => DeliveryOutcome::Replaced,
                },
                cancelled,
            ),
        }
    }

    fn complete(self, session: &Session) -> Result<(), OpsError> {
        match self {
            Self::Messages(pending) => ops::complete_read(session, &pending),
            Self::Handoff(pending) => ops::observe_handoff(session, &pending),
        }
    }
}

struct DeliveryCompletion {
    session: Session,
    pending: PendingDelivery,
}

struct CallReply {
    value: Value,
    completion: Option<DeliveryCompletion>,
}

impl From<Value> for CallReply {
    fn from(value: Value) -> Self {
        Self {
            value,
            completion: None,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn client_tool(params: &Value) -> Option<Tool> {
    let name = params.pointer("/clientInfo/name")?.as_str()?.to_lowercase();
    if name.contains("claude") {
        Some(Tool::Claude)
    } else if name.contains("codex") {
        Some(Tool::Codex)
    } else if name.contains("opencode") {
        Some(Tool::Opencode)
    } else {
        None
    }
}

fn rendered_outcome_using(
    outcome: &Outcome,
    unread: impl FnOnce() -> Result<Option<usize>, OpsError>,
) -> Value {
    let mut value = outcome.json();
    if outcome.pending_read().is_some()
        || matches!(
            outcome,
            Outcome::HandoffCreated(_) | Outcome::HandoffMetadata { .. } | Outcome::HandoffBody(_)
        )
    {
        return value;
    }
    if let (Value::Object(fields), Ok(Some(count))) = (&mut value, unread()) {
        fields.insert("unread".to_owned(), json!(count));
    }
    value
}

impl<W: Write + Send> Server<W> {
    fn send(&self, value: &Value) -> io::Result<ResponseDelivery> {
        let mut bytes = serde_json::to_vec(value)?;
        let delivered = if bytes.len() > MAX_CONTROL_BYTES {
            bytes = serde_json::to_vec(&error(
                value.get("id").unwrap_or(&Value::Null),
                INTERNAL_ERROR,
                "The result exceeds 1 MiB; request fewer events",
            ))?;
            ResponseDelivery::Replaced
        } else {
            ResponseDelivery::Original
        };
        let mut output = lock(&self.output);
        output.write_all(&bytes)?;
        output.write_all(b"\n")?;
        output.flush()?;
        drop(output);
        Ok(delivered)
    }

    fn initialize(&self, id: &Value, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let valid = requested.is_some()
            && params.get("capabilities").is_some_and(Value::is_object)
            && params
                .pointer("/clientInfo/name")
                .is_some_and(Value::is_string);
        {
            let mut phase = lock(&self.phase);
            if !valid || *phase != Phase::New {
                return error(id, INVALID_PARAMS, "Invalid initialization");
            }
            *phase = Phase::Negotiated;
        }
        *lock(&self.client) = client_tool(params);
        let version = requested
            .filter(|requested| VERSIONS.contains(requested))
            .unwrap_or(VERSIONS[0]);
        result(
            id,
            &json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "domyjob", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }),
        )
    }

    fn call(&self, id: &Value, params: Value, cancelled: &Arc<AtomicBool>) -> Option<CallReply> {
        let Ok(call) = decode::<Call>(params) else {
            return Some(error(id, INVALID_PARAMS, "Invalid tool parameters").into());
        };
        let mut session = lock(&self.session).clone_for(Arc::clone(cancelled));
        let client = *lock(&self.client);
        let outcome = dispatch(&mut session, &call.name, call.arguments, client);
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        if call.name == "chat_join" && outcome.is_ok() {
            lock(&self.session).adopt(&session);
        }
        let (value, pending) = match outcome {
            Ok(outcome) => {
                let pending = outcome.pending_read().cloned().map_or_else(
                    || {
                        outcome
                            .pending_handoff()
                            .cloned()
                            .map(|pending| PendingDelivery::Handoff(Box::new(pending)))
                    },
                    |pending| Some(PendingDelivery::Messages(Box::new(pending))),
                );
                let value = rendered_outcome_using(&outcome, || ops::unread(&session));
                (
                    result(
                        id,
                        &json!({"content": [{"type": "text", "text": value.to_string()}], "structuredContent": value, "isError": false}),
                    ),
                    pending,
                )
            }
            Err(CallError::Unknown) => (error(id, INVALID_PARAMS, "Unknown tool"), None),
            Err(CallError::Arguments(problem)) => (
                error(id, INVALID_PARAMS, &format!("Invalid arguments: {problem}")),
                None,
            ),
            Err(CallError::Ops(problem)) => (
                result(
                    id,
                    &json!({"content": [{"type": "text", "text": problem.to_string()}], "isError": true}),
                ),
                None,
            ),
        };
        Some(CallReply {
            value,
            completion: pending.map(|pending| DeliveryCompletion { session, pending }),
        })
    }

    fn deliver_call(&self, reply: CallReply, cancelled: &AtomicBool) -> Result<(), McpError> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let delivered = self.send(&reply.value)?;
        let _calls = lock(&self.calls);
        if let Some(completion) = reply.completion
            && completion
                .pending
                .admits(delivered, cancelled.load(Ordering::Acquire))
        {
            completion.pending.complete(&completion.session)?;
        }
        Ok(())
    }

    fn respond(&self, id: &Value, method: &str, params: &Value) -> Value {
        let ready = *lock(&self.phase) == Phase::Ready;
        match method {
            "initialize" => self.initialize(id, params),
            "ping" => result(id, &json!({})),
            "tools/list" if ready => result(id, &json!({"tools": catalog()})),
            "tools/list" | "tools/call" => {
                error(id, SERVER_NOT_INITIALIZED, "Server not initialized")
            }
            _ => error(id, METHOD_NOT_FOUND, "Method not found"),
        }
    }
}

fn key(id: &Value) -> String {
    id.to_string()
}

fn read_line(input: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let limit = u64::try_from(MAX_CONTROL_BYTES)
        .map_err(io::Error::other)?
        .saturating_add(1);
    let mut line = Vec::new();
    if io::Read::take(&mut *input, limit).read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn serve_io<W: Write + Send>(mut input: impl BufRead, server: &Server<W>) -> io::Result<()> {
    std::thread::scope(|scope| -> io::Result<()> {
        while let Some(line) = read_line(&mut input)? {
            if line.len() > MAX_CONTROL_BYTES {
                server.send(&error(
                    &Value::Null,
                    INVALID_REQUEST,
                    "Request exceeds 1 MiB",
                ))?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized MCP request",
                ));
            }
            let request = match parse(&line) {
                Ok(request) => request,
                Err(problem) => {
                    server.send(&problem)?;
                    continue;
                }
            };
            let Some(id) = request.id else {
                notification(server, &request.method, &request.params);
                continue;
            };
            if request.method != "tools/call" || *lock(&server.phase) != Phase::Ready {
                server.send(&server.respond(&id, &request.method, &request.params))?;
                continue;
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            let refusal = {
                let mut calls = lock(&server.calls);
                if calls.contains_key(&key(&id)) {
                    Some("request id is already active")
                } else if calls.len() >= MAX_CALLS {
                    Some("too many active requests")
                } else {
                    calls.insert(key(&id), Arc::clone(&cancelled));
                    None
                }
            };
            if let Some(message) = refusal {
                server.send(&error(&id, REQUEST_REFUSED, message))?;
                continue;
            }
            scope.spawn(move || {
                let reply = server.call(&id, request.params, &cancelled);
                if let Some(reply) = reply {
                    let _written = server.deliver_call(reply, &cancelled);
                }
                lock(&server.calls).remove(&key(&id));
            });
        }
        Ok(())
    })
}

fn notification<W>(server: &Server<W>, method: &str, params: &Value) {
    match method {
        "notifications/initialized" => {
            let mut phase = lock(&server.phase);
            if *phase == Phase::Negotiated {
                *phase = Phase::Ready;
            }
        }
        "notifications/cancelled" => {
            if let Some(id) = params.get("requestId")
                && let Some(flag) = lock(&server.calls).get(&key(id))
            {
                flag.store(true, Ordering::Release);
            }
        }
        _ => {}
    }
}

pub(crate) fn serve(
    output: Output,
    actor: Option<&str>,
    turn: Option<&str>,
) -> Result<(), McpError> {
    let session = Session::open(actor, turn)?;
    let server = Server {
        output: Mutex::new(output.into_stream()),
        phase: Mutex::new(Phase::New),
        session: Mutex::new(session),
        client: Mutex::new(None),
        calls: Mutex::new(BTreeMap::new()),
    };
    serve_io(io::stdin().lock(), &server)?;
    Ok(())
}

#[cfg(test)]
mod tests;
