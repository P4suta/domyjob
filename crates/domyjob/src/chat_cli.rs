#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root owns the chat adapter"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::chat::{
    AgentTool, Audience, ChatError, Event, EventData, MessageMode, Store, TurnFailure, direct,
};

#[derive(Debug, Args)]
pub(crate) struct ChatArgs {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: ChatCommand,
}

#[derive(Debug, Subcommand)]
enum ChatCommand {
    Setup {
        machines: Vec<String>,
    },
    Doctor,
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Room {
        #[command(subcommand)]
        command: RoomCommand,
    },
    Send(SendArgs),
    Ask(AskArgs),
    Reply(ReplyArgs),
    Inbox,
    Thread(TargetArgs),
    Watch(TargetArgs),
    Open(TargetArgs),
    Sync {
        machine: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    Start(StartArgs),
    Attach(AttachArgs),
    List,
}

#[derive(Debug, Subcommand)]
enum RoomCommand {
    Create(CreateArgs),
    Add(MemberArgs),
    Remove(MemberArgs),
    List,
}

#[derive(Debug, Clone, Copy, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ToolKind {
    Claude,
    Codex,
    Opencode,
}

impl From<ToolKind> for AgentTool {
    fn from(kind: ToolKind) -> Self {
        match kind {
            ToolKind::Claude => Self::Claude,
            ToolKind::Codex => Self::Codex,
            ToolKind::Opencode => Self::Opencode,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    target: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    target: String,
    text: String,
    #[arg(long)]
    from: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    target: String,
    text: String,
    #[arg(long)]
    to: Option<String>,
    #[arg(long)]
    from: Option<String>,
    #[arg(long, default_value_t = 120)]
    #[serde(default = "default_timeout")]
    timeout: u64,
}

const fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    message: String,
    text: String,
    #[arg(long)]
    from: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    name: String,
    #[arg(long)]
    kind: ToolKind,
    #[arg(long)]
    cwd: PathBuf,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AttachArgs {
    #[command(flatten)]
    #[serde(flatten)]
    agent: StartArgs,
    #[arg(long)]
    session: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    name: String,
    members: Vec<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemberArgs {
    room: String,
    member: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChatCliError {
    #[error(transparent)]
    Chat(#[from] ChatError),
    #[error(transparent)]
    Sync(#[from] crate::chat_sync::SyncError),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("reading input or writing output: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding or decoding JSON: {0}")]
    Json(#[from] serde_json::Error),
}

struct Context<'a> {
    store: &'a Store,
    machines: BTreeMap<String, String>,
}

impl<'a> Context<'a> {
    fn new(store: &'a Store) -> Result<Self, ChatCliError> {
        let mut machines = crate::chat_sync::peers()?;
        machines.insert("local".to_owned(), store.origin().to_owned());
        Ok(Self { store, machines })
    }

    fn agent_id(&self, text: &str) -> Result<String, ChatCliError> {
        let canonical = text.rsplit_once('@').and_then(|(name, machine)| {
            self.machines
                .get(machine)
                .map(|origin| format!("{name}@{origin}"))
        });
        let mut agents: BTreeSet<String> = self.store.agents()?.into_keys().collect();
        agents.extend(
            self.machines
                .values()
                .map(|origin| format!("owner@{origin}")),
        );
        if text == "owner" {
            return Ok(format!("owner@{}", self.store.origin()));
        }
        let matches: Vec<String> = agents
            .into_iter()
            .filter(|id| {
                text == id
                    || canonical.as_ref() == Some(id)
                    || id.rsplit_once('@').is_some_and(|(name, _)| text == name)
            })
            .collect();
        unique_name(&matches, text)
    }

    fn room_id(&self, text: &str) -> Result<String, ChatCliError> {
        let matches: Vec<_> = self
            .store
            .rooms()?
            .into_values()
            .filter(|room| {
                room.id == text
                    || room.name == text
                    || text.rsplit_once('@').is_some_and(|(name, machine)| {
                        name == room.name && self.machines.get(machine) == Some(&room.owner)
                    })
            })
            .map(|room| room.id)
            .collect();
        unique_name(&matches, text)
    }

    fn sender_id(&self, from: Option<&str>) -> Result<String, ChatCliError> {
        let environment = match std::env::var("DOMYJOB_CHAT_AGENT") {
            Ok(name) => Some(name),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(ChatCliError::Invalid("DOMYJOB_CHAT_AGENT must be UTF-8"));
            }
        };
        let id = self.agent_id(from.or(environment.as_deref()).unwrap_or("owner"))?;
        if !id.ends_with(&format!("@{}", self.store.origin())) {
            return Err(ChatCliError::Invalid("the sender must be a local agent"));
        }
        Ok(id)
    }

    fn destination(
        &self,
        target: &str,
        from: &str,
    ) -> Result<(String, Option<String>), ChatCliError> {
        match self.room_id(target) {
            Ok(room) => {
                if !self
                    .store
                    .rooms()?
                    .get(&room)
                    .is_some_and(|view| view.members.contains(from))
                {
                    return Err(ChatCliError::Invalid("the sender is not in this room"));
                }
                Ok((room, None))
            }
            Err(ChatCliError::Chat(ChatError::Unknown(_))) => {
                let receiver = self.agent_id(target)?;
                Ok((direct(from, &receiver), Some(receiver)))
            }
            Err(error) => Err(error),
        }
    }

    fn thread(&self, target: &str) -> Result<String, ChatCliError> {
        let from = self.sender_id(None)?;
        self.destination(target, &from).map(|(room, _)| room)
    }
}

fn unique_name(matches: &[String], text: &str) -> Result<String, ChatCliError> {
    match matches {
        [only] => Ok(only.clone()),
        [] => Err(ChatError::Unknown(text.to_owned()).into()),
        _ => Err(ChatError::Ambiguous(text.to_owned()).into()),
    }
}

fn audience(
    store: &Store,
    to: &str,
    receiver: Option<&str>,
    sender: &str,
) -> Result<Audience, ChatCliError> {
    let members = if let Some(receiver) = receiver {
        vec![sender.to_owned(), receiver.to_owned()]
    } else {
        store
            .rooms()?
            .get(to)
            .ok_or_else(|| ChatError::Unknown(to.to_owned()))?
            .members
            .iter()
            .cloned()
            .collect()
    };
    let origins = members
        .iter()
        .map(|member| {
            member
                .rsplit_once('@')
                .map(|(_, origin)| origin.to_owned())
                .ok_or(ChatCliError::Invalid("a member has no machine identity"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(Audience::try_from(origins.into_iter().collect::<Vec<_>>()).map_err(ChatError::from)?)
}

enum OutboundMode {
    Send,
    Ask { responder: Option<String> },
}

fn send(context: &Context<'_>, args: SendArgs, mode: OutboundMode) -> Result<Event, ChatCliError> {
    let from = context.sender_id(args.from.as_deref())?;
    let (to, receiver) = context.destination(&args.target, &from)?;
    let mode = match mode {
        OutboundMode::Send => MessageMode::Send,
        OutboundMode::Ask { responder } => MessageMode::Ask {
            responder: responder
                .or_else(|| receiver.clone())
                .ok_or(ChatCliError::Invalid("a room ask requires --to AGENT"))?,
        },
    };
    if let MessageMode::Ask { responder } = &mode {
        if let Some(receiver) = receiver.as_deref() {
            if responder != receiver {
                return Err(ChatCliError::Invalid(
                    "the asked agent is not the direct recipient",
                ));
            }
        } else if !context
            .store
            .rooms()?
            .get(&to)
            .is_some_and(|room| room.members.contains(responder))
        {
            return Err(ChatCliError::Invalid("the asked agent is not in the room"));
        }
    }
    let audience = audience(context.store, &to, receiver.as_deref(), &from)?;
    Ok(context.store.append(EventData::Message {
        to,
        from,
        text: args.text,
        audience,
        mode,
    })?)
}

fn reply(context: &Context<'_>, args: ReplyArgs) -> Result<Event, ChatCliError> {
    let from = context.sender_id(args.from.as_deref())?;
    let original = context
        .store
        .event(&args.message)?
        .ok_or_else(|| ChatError::Unknown(args.message.clone()))?;
    let EventData::Message {
        to,
        from: original_sender,
        mode,
        audience,
        ..
    } = original.data
    else {
        return Err(ChatCliError::Invalid("the reply target is not a message"));
    };
    if matches!(&mode, MessageMode::Ask { responder } if responder != &from) {
        return Err(ChatCliError::Invalid("another agent was asked to reply"));
    }
    if to.starts_with("dm:") && direct(&original_sender, &from) != to {
        return Err(ChatCliError::Invalid(
            "the sender is not in this direct chat",
        ));
    }
    if !matches!(&mode, MessageMode::Ask { .. })
        && to.starts_with("room:")
        && !context
            .store
            .rooms()?
            .get(&to)
            .is_some_and(|room| room.members.contains(&from))
    {
        return Err(ChatCliError::Invalid("the sender is not in this room"));
    }
    let answer = EventData::Message {
        to,
        from,
        text: args.text,
        audience,
        mode: MessageMode::Reply {
            request: args.message,
        },
    };
    Ok(if matches!(&mode, MessageMode::Ask { .. }) {
        context.store.append_resolution(answer)?
    } else {
        context.store.append(answer)?
    })
}

fn message_events(store: &Store, room: &str) -> Result<Vec<Event>, ChatCliError> {
    Ok(store
        .events()?
        .into_iter()
        .filter(|event| event.data.conversation() == Some(room))
        .collect())
}

fn inbox_events(store: &Store, recipient: &str) -> Result<Vec<Event>, ChatCliError> {
    let rooms = store.rooms()?;
    let includes_recipient = |to: &str, from: &str| {
        from != recipient
            && (direct(from, recipient) == to
                || rooms
                    .get(to)
                    .is_some_and(|room| room.members.contains(recipient)))
    };
    Ok(store
        .events()?
        .into_iter()
        .filter(|event| match &event.data {
            EventData::Message {
                to, from, audience, ..
            } => audience.includes(store.origin()) && includes_recipient(to, from),
            EventData::TurnFailure {
                to,
                agent,
                audience,
                ..
            } => audience.includes(store.origin()) && includes_recipient(to, agent),
            EventData::Agent { .. }
            | EventData::Room { .. }
            | EventData::Membership { .. }
            | EventData::Omitted {} => false,
        })
        .collect())
}

enum Answer {
    Replied(Event),
    Failed(TurnFailure),
    Pending,
}

fn answer(store: &Store, id: &str) -> Result<Answer, ChatCliError> {
    for event in store.events()? {
        match &event.data {
            EventData::Message {
                mode: MessageMode::Reply { request },
                ..
            } if request == id => return Ok(Answer::Replied(event)),
            EventData::TurnFailure {
                request, failure, ..
            } if request == id => return Ok(Answer::Failed(*failure)),
            EventData::Message { .. }
            | EventData::TurnFailure { .. }
            | EventData::Agent { .. }
            | EventData::Room { .. }
            | EventData::Membership { .. }
            | EventData::Omitted {} => {}
        }
    }
    Ok(Answer::Pending)
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Synchronization {
    Report(crate::chat_sync::SyncReport),
    Failed { complete: bool, error: String },
}

impl Synchronization {
    fn attempt(store: &Store) -> Self {
        match crate::chat_sync::sync(store, None) {
            Ok(report) => Self::Report(report),
            Err(error) => Self::Failed {
                complete: false,
                error: error.to_string(),
            },
        }
    }

    fn complete(&self) -> bool {
        match self {
            Self::Report(report) => report.is_complete(),
            Self::Failed { .. } => false,
        }
    }
}

#[derive(Debug)]
enum Action {
    Agents(EmptyArgs),
    Rooms(EmptyArgs),
    Inbox(EmptyArgs),
    Thread(TargetArgs),
    Send(SendArgs),
    Ask(AskArgs),
    Reply(ReplyArgs),
    Start(StartArgs),
    Attach(AttachArgs),
    Create(CreateArgs),
    Add(MemberArgs),
    Remove(MemberArgs),
}

enum OutcomeData {
    Event(Event),
    Events(Vec<Event>),
    Agents(BTreeMap<String, Event>),
    Rooms(BTreeMap<String, crate::chat::RoomView>),
    Ask { request: Event, answer: Answer },
}

struct Outcome {
    data: OutcomeData,
    sync: Synchronization,
}

impl Outcome {
    fn value(&self) -> Value {
        let mut value = match &self.data {
            OutcomeData::Event(event) => json!({ "message_id": event.id(), "event": event }),
            OutcomeData::Events(events) => json!({ "events": events }),
            OutcomeData::Agents(agents) => json!({ "agents": agents }),
            OutcomeData::Rooms(rooms) => json!({ "rooms": rooms }),
            OutcomeData::Ask { request, answer } => match answer {
                Answer::Replied(reply) => {
                    json!({ "message_id": request.id(), "state": "answered", "reply": reply })
                }
                Answer::Failed(failure) => {
                    json!({ "message_id": request.id(), "state": failure.as_str() })
                }
                Answer::Pending => json!({ "message_id": request.id(), "state": "pending" }),
            },
        };
        if let Value::Object(fields) = &mut value {
            fields.insert("sync".to_owned(), json!(self.sync));
        }
        value
    }

    fn emit(&self, json_mode: bool) -> Result<(), ChatCliError> {
        if json_mode {
            writeln!(std::io::stdout(), "{}", self.value())?;
            return Ok(());
        }
        match &self.data {
            OutcomeData::Event(event) => println!("{}", event.id()),
            OutcomeData::Events(events) => print_events(false, events)?,
            OutcomeData::Agents(agents) => {
                for id in agents.keys() {
                    println!("{}", neutralize(id));
                }
            }
            OutcomeData::Rooms(rooms) => {
                for room in rooms.values() {
                    println!("{} ({})", neutralize(&room.name), neutralize(&room.id));
                }
            }
            OutcomeData::Ask { request, answer } => match answer {
                Answer::Replied(reply) => print_events(false, std::slice::from_ref(reply))?,
                Answer::Failed(failure) => println!("{} {}", request.id(), failure.as_str()),
                Answer::Pending => println!("{} is pending", request.id()),
            },
        }
        if !self.sync.complete() {
            eprintln!("chat synchronization incomplete: {}", json!(self.sync));
        }
        Ok(())
    }

    fn status(&self) -> ExitCode {
        match &self.data {
            OutcomeData::Ask {
                answer: Answer::Pending,
                ..
            } => ExitCode::from(3),
            OutcomeData::Ask {
                answer: Answer::Failed(_),
                ..
            } => ExitCode::FAILURE,
            OutcomeData::Ask {
                answer: Answer::Replied(_),
                ..
            }
            | OutcomeData::Event(_)
            | OutcomeData::Events(_)
            | OutcomeData::Agents(_)
            | OutcomeData::Rooms(_) => ExitCode::SUCCESS,
        }
    }
}

fn register(
    store: &Store,
    args: StartArgs,
    session: Option<String>,
) -> Result<Event, ChatCliError> {
    if !args.cwd.is_absolute() || !std::fs::metadata(&args.cwd)?.is_dir() {
        return Err(ChatCliError::Invalid(
            "the agent directory must be an existing absolute directory",
        ));
    }
    if session
        .as_ref()
        .is_some_and(|session| session.trim().is_empty())
    {
        return Err(ChatCliError::Invalid("the session must not be empty"));
    }
    if store
        .agents()?
        .contains_key(&format!("{}@{}", args.name, store.origin()))
    {
        return Err(ChatCliError::Invalid(
            "an agent with this name already exists here",
        ));
    }
    let cwd = args
        .cwd
        .into_os_string()
        .into_string()
        .map_err(|_invalid| ChatCliError::Invalid("the agent directory must be UTF-8"))?;
    Ok(store.append(EventData::Agent {
        name: args.name,
        tool: args.kind.into(),
        cwd,
        managed: session.is_none(),
        session,
    })?)
}

fn create_room(context: &Context<'_>, args: CreateArgs) -> Result<Event, ChatCliError> {
    if context
        .store
        .rooms()?
        .values()
        .any(|room| room.name == args.name && room.owner == context.store.origin())
    {
        return Err(ChatCliError::Invalid(
            "a room with this name already exists here",
        ));
    }
    let mut members = BTreeSet::from([format!("owner@{}", context.store.origin())]);
    for member in args.members {
        members.insert(context.agent_id(&member)?);
    }
    Ok(context.store.append(EventData::Room {
        id: format!("room:{}:{}", context.store.origin(), args.name),
        name: args.name,
        members: members.into_iter().collect(),
    })?)
}

fn membership(
    context: &Context<'_>,
    args: &MemberArgs,
    present: bool,
) -> Result<Event, ChatCliError> {
    let room = context.room_id(&args.room)?;
    if !room.starts_with(&format!("room:{}:", context.store.origin())) {
        return Err(ChatCliError::Invalid(
            "only the room creator may change members",
        ));
    }
    Ok(context.store.append(EventData::Membership {
        room,
        member: context.agent_id(&args.member)?,
        present,
    })?)
}

fn ask(context: &Context<'_>, args: AskArgs) -> Result<Outcome, ChatCliError> {
    if args.timeout > 600 {
        return Err(ChatCliError::Invalid("timeout must be at most 600 seconds"));
    }
    let responder = args.to.map(|name| context.agent_id(&name)).transpose()?;
    let request = send(
        context,
        SendArgs {
            target: args.target,
            text: args.text,
            from: args.from,
        },
        OutboundMode::Ask { responder },
    )?;
    let window = crate::chat_sync::WaitWindow::new(args.timeout);
    let mut sync = Synchronization::attempt(context.store);
    loop {
        let answer = answer(context.store, &request.id())?;
        if !matches!(answer, Answer::Pending) {
            return Ok(Outcome {
                data: OutcomeData::Ask { request, answer },
                sync,
            });
        }
        if window.expired() {
            break;
        }
        window.wait_tick();
        sync = Synchronization::attempt(context.store);
    }
    let answer = answer(context.store, &request.id())?;
    Ok(Outcome {
        data: OutcomeData::Ask { request, answer },
        sync,
    })
}

fn perform(context: &Context<'_>, action: Action) -> Result<Outcome, ChatCliError> {
    let synchronization = Synchronization::attempt(context.store);
    let read = match &action {
        Action::Agents(_) | Action::Rooms(_) | Action::Inbox(_) | Action::Thread(_) => true,
        Action::Ask(_)
        | Action::Send(_)
        | Action::Reply(_)
        | Action::Start(_)
        | Action::Attach(_)
        | Action::Create(_)
        | Action::Add(_)
        | Action::Remove(_) => false,
    };
    let data = match action {
        Action::Agents(_) => OutcomeData::Agents(context.store.agents()?),
        Action::Rooms(_) => OutcomeData::Rooms(context.store.rooms()?),
        Action::Inbox(_) => {
            OutcomeData::Events(inbox_events(context.store, &context.sender_id(None)?)?)
        }
        Action::Thread(args) => OutcomeData::Events(message_events(
            context.store,
            &context.thread(&args.target)?,
        )?),
        Action::Send(args) => OutcomeData::Event(send(context, args, OutboundMode::Send)?),
        Action::Reply(args) => OutcomeData::Event(reply(context, args)?),
        Action::Start(args) => OutcomeData::Event(register(context.store, args, None)?),
        Action::Attach(args) => {
            OutcomeData::Event(register(context.store, args.agent, Some(args.session))?)
        }
        Action::Create(args) => OutcomeData::Event(create_room(context, args)?),
        Action::Add(args) => OutcomeData::Event(membership(context, &args, true)?),
        Action::Remove(args) => OutcomeData::Event(membership(context, &args, false)?),
        Action::Ask(args) => return ask(context, args),
    };
    Ok(Outcome {
        data,
        sync: if read {
            synchronization
        } else {
            Synchronization::attempt(context.store)
        },
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "MCP arguments are decoded at one strict adapter boundary"
)]
fn arguments<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ChatCliError> {
    Ok(serde_json::from_value(input)?)
}

macro_rules! chat_tools {
    ($(($name:literal, $description:literal, $variant:ident, $args:ty)),+ $(,)?) => {
        pub(crate) fn tools() -> Vec<Value> {
            vec![$(json!({ "name": $name, "description": $description, "inputSchema": schemars::schema_for!($args) })),+]
        }

        fn decode_action(name: &str, input: Value) -> Result<Action, ChatCliError> {
            match name {
                $($name => Ok(Action::$variant(arguments::<$args>(input)?)),)+
                _ => Err(ChatCliError::Invalid("unknown chat tool")),
            }
        }
    };
}

chat_tools!(
    (
        "chat_agents",
        "List registered AI agents.",
        Agents,
        EmptyArgs
    ),
    (
        "chat_rooms",
        "List chat rooms and membership.",
        Rooms,
        EmptyArgs
    ),
    (
        "chat_inbox",
        "Read messages received on this machine.",
        Inbox,
        EmptyArgs
    ),
    (
        "chat_thread",
        "Read a direct conversation or room.",
        Thread,
        TargetArgs
    ),
    (
        "chat_send",
        "Persist and synchronize a message.",
        Send,
        SendArgs
    ),
    (
        "chat_ask",
        "Ask one agent and wait for its durable answer.",
        Ask,
        AskArgs
    ),
    (
        "chat_reply",
        "Reply to an exact message ID.",
        Reply,
        ReplyArgs
    ),
    (
        "chat_agent_start",
        "Register a managed AI agent.",
        Start,
        StartArgs
    ),
    (
        "chat_agent_attach",
        "Register an existing interactive AI session.",
        Attach,
        AttachArgs
    ),
    (
        "chat_room_create",
        "Create a room with its initial members.",
        Create,
        CreateArgs
    ),
    (
        "chat_room_add",
        "Add a member to a room owned here.",
        Add,
        MemberArgs
    ),
    (
        "chat_room_remove",
        "Remove a member from a room owned here.",
        Remove,
        MemberArgs
    ),
);

pub(crate) fn mcp_call(name: &str, input: Value) -> Result<Value, ChatCliError> {
    let action = decode_action(name, input)?;
    let store = Store::open()?;
    let context = Context::new(&store)?;
    Ok(perform(&context, action)?.value())
}

fn neutralize(text: &str) -> String {
    domyjob_core::domain::terminal_text(text)
}

fn print_events(json_mode: bool, events: &[Event]) -> Result<(), ChatCliError> {
    if json_mode {
        writeln!(std::io::stdout(), "{}", json!({ "events": events }))?;
    } else {
        for event in events {
            match &event.data {
                EventData::Message { from, text, .. } => {
                    println!("{} {}: {}", event.id(), neutralize(from), neutralize(text));
                }
                EventData::TurnFailure {
                    request, failure, ..
                } => println!("{} {}", neutralize(request), failure.as_str()),
                EventData::Agent { .. }
                | EventData::Room { .. }
                | EventData::Membership { .. }
                | EventData::Omitted {} => {}
            }
        }
    }
    Ok(())
}

fn sync_visible(store: &Store) {
    let synchronization = Synchronization::attempt(store);
    if !synchronization.complete() {
        eprintln!(
            "chat synchronization incomplete: {}",
            json!(synchronization)
        );
    }
}

fn watch(context: &Context<'_>, target: &str, json_mode: bool) -> Result<(), ChatCliError> {
    sync_visible(context.store);
    let room = context.thread(target)?;
    let mut seen = BTreeSet::new();
    loop {
        sync_visible(context.store);
        for event in message_events(context.store, &room)? {
            if seen.insert(event.id()) {
                print_events(json_mode, &[event])?;
            }
        }
        crate::chat_sync::wait_tick();
    }
}

fn open(context: &Context<'_>, target: &str) -> Result<(), ChatCliError> {
    sync_visible(context.store);
    let room = context.thread(target)?;
    let mut seen = BTreeSet::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    let _reader = std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    loop {
        for event in message_events(context.store, &room)? {
            if seen.insert(event.id()) {
                print_events(false, &[event])?;
            }
        }
        match receiver.try_recv() {
            Ok(Ok(line)) if line == "/quit" => break,
            Ok(Ok(line)) if !line.is_empty() => {
                let outcome = perform(
                    context,
                    Action::Send(SendArgs {
                        target: target.to_owned(),
                        text: line,
                        from: None,
                    }),
                )?;
                outcome.emit(false)?;
                std::io::stdout().flush()?;
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Ok(Ok(_)) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        sync_visible(context.store);
        crate::chat_sync::wait_tick();
    }
    Ok(())
}

fn setup(machines: &[String]) -> Result<(), ChatCliError> {
    crate::chat_sync::setup(machines)?;
    let store = Store::open()?;
    let synchronization = Synchronization::attempt(&store);
    if !synchronization.complete() {
        eprintln!(
            "chat synchronization incomplete: {}",
            json!(synchronization)
        );
    }
    let exe = std::env::current_exe()?;
    let config = json!({ "mcpServers": { "domyjob": { "command": exe, "args": ["mcp"] } } });
    writeln!(std::io::stdout(), "{config}")?;
    Ok(())
}

fn doctor(context: &Context<'_>, json_mode: bool) -> Result<ExitCode, ChatCliError> {
    let mut findings = Vec::new();
    for (id, event) in context.store.agents()? {
        if event.origin != context.store.origin() {
            continue;
        }
        if let EventData::Agent { cwd, .. } = event.data {
            match std::fs::metadata(&cwd) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => findings.push(format!("Agent {id} has no working directory.")),
                Err(error) => findings.push(format!("Agent {id}: {error}")),
            }
        }
    }
    let sync = Synchronization::attempt(context.store);
    let ok = findings.is_empty() && sync.complete();
    if json_mode {
        writeln!(
            std::io::stdout(),
            "{}",
            json!({ "ok": ok, "findings": findings, "sync": sync })
        )?;
    } else {
        for finding in findings {
            println!("{}", neutralize(&finding));
        }
        println!("Chat synchronization: {}", json!(sync));
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(crate) fn run(args: ChatArgs) -> Result<ExitCode, ChatCliError> {
    let store = Store::open()?;
    let context = Context::new(&store)?;
    let action = match args.command {
        ChatCommand::Setup { machines } => {
            setup(&machines)?;
            return Ok(ExitCode::SUCCESS);
        }
        ChatCommand::Doctor => return doctor(&context, args.json),
        ChatCommand::Sync { machine } => {
            let report = crate::chat_sync::sync(&store, machine.as_deref())?;
            writeln!(std::io::stdout(), "{}", json!(report))?;
            return Ok(if report.is_complete() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        ChatCommand::Watch(target) => {
            watch(&context, &target.target, args.json)?;
            return Ok(ExitCode::SUCCESS);
        }
        ChatCommand::Open(target) => {
            open(&context, &target.target)?;
            return Ok(ExitCode::SUCCESS);
        }
        ChatCommand::Agent {
            command: AgentCommand::Start(args),
        } => Action::Start(args),
        ChatCommand::Agent {
            command: AgentCommand::Attach(args),
        } => Action::Attach(args),
        ChatCommand::Agent {
            command: AgentCommand::List,
        } => Action::Agents(EmptyArgs {}),
        ChatCommand::Room {
            command: RoomCommand::Create(args),
        } => Action::Create(args),
        ChatCommand::Room {
            command: RoomCommand::Add(args),
        } => Action::Add(args),
        ChatCommand::Room {
            command: RoomCommand::Remove(args),
        } => Action::Remove(args),
        ChatCommand::Room {
            command: RoomCommand::List,
        } => Action::Rooms(EmptyArgs {}),
        ChatCommand::Send(args) => Action::Send(args),
        ChatCommand::Ask(args) => Action::Ask(args),
        ChatCommand::Reply(args) => Action::Reply(args),
        ChatCommand::Inbox => Action::Inbox(EmptyArgs {}),
        ChatCommand::Thread(args) => Action::Thread(args),
    };
    let outcome = perform(&context, action)?;
    outcome.emit(args.json)?;
    Ok(outcome.status())
}

#[cfg(test)]
mod tests {
    use super::{
        Action, AttachArgs, ChatCliError, Context, OutboundMode, ReplyArgs, SendArgs, StartArgs,
        ToolKind, arguments, decode_action, inbox_events, register, reply, send, tools,
    };
    use crate::chat::{ChatError, EventData, MessageMode, Store};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn owners_resolve_without_registration_and_agent_names_remain_unambiguous() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        let context = Context {
            store: &store,
            machines: BTreeMap::from([
                ("local".to_owned(), "a".to_owned()),
                ("linux".to_owned(), "b".to_owned()),
            ]),
        };
        assert_eq!(context.agent_id("owner").unwrap(), "owner@a");
        assert_eq!(context.agent_id("owner@linux").unwrap(), "owner@b");
        assert_eq!(context.agent_id("owner@b").unwrap(), "owner@b");
        assert!(matches!(
            context.agent_id("owner@unknown"),
            Err(ChatCliError::Chat(ChatError::Unknown(_)))
        ));
    }

    #[test]
    fn mcp_catalog_and_decoder_share_strict_input_shapes() {
        let names: std::collections::BTreeSet<_> = tools()
            .into_iter()
            .map(|tool| tool.get("name").unwrap().as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names.len(), 12);
        assert!(
            matches!(decode_action("chat_ask", json!({"target":"owner", "text":"hello"})).unwrap(), Action::Ask(args) if args.timeout == 120)
        );
        for value in [
            json!({"name":"a","kind":"codex","cwd":"/tmp","session":"id","unexpected":true}),
            json!({"name":"a","kind":"unknown","cwd":"/tmp","session":"id"}),
        ] {
            arguments::<AttachArgs>(value).unwrap_err();
        }
        arguments::<AttachArgs>(json!({"name":"a","kind":"codex","cwd":"/tmp","session":"id"}))
            .unwrap();
        decode_action("chat_inbox", json!({"unexpected":true})).unwrap_err();
        decode_action("missing", json!({})).unwrap_err();
    }

    #[test]
    fn local_ask_requires_the_exact_responder_and_commits_only_one_answer() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        register(
            &store,
            StartArgs {
                name: "reviewer".to_owned(),
                kind: ToolKind::Codex,
                cwd: root.path().to_owned(),
            },
            Some("session".to_owned()),
        )
        .unwrap();
        let context = Context {
            store: &store,
            machines: BTreeMap::from([("local".to_owned(), "a".to_owned())]),
        };
        let request = send(
            &context,
            SendArgs {
                target: "reviewer".to_owned(),
                text: "review".to_owned(),
                from: Some("owner".to_owned()),
            },
            OutboundMode::Ask { responder: None },
        )
        .unwrap();
        assert!(matches!(
            request.data,
            EventData::Message {
                mode: MessageMode::Ask { .. },
                ..
            }
        ));
        assert!(inbox_events(&store, "owner@a").unwrap().is_empty());
        assert_eq!(
            inbox_events(&store, "reviewer@a").unwrap(),
            vec![request.clone()]
        );
        assert!(inbox_events(&store, "unrelated@a").unwrap().is_empty());
        let arguments = |from: &str| ReplyArgs {
            message: request.id(),
            text: "done".to_owned(),
            from: Some(from.to_owned()),
        };
        reply(&context, arguments("owner")).unwrap_err();
        let response = reply(&context, arguments("reviewer")).unwrap();
        assert_eq!(inbox_events(&store, "owner@a").unwrap(), vec![response]);
        assert!(matches!(
            reply(&context, arguments("reviewer")),
            Err(ChatCliError::Chat(ChatError::AlreadyResolved(_)))
        ));
    }

    #[test]
    fn attached_sessions_reject_empty_ids_and_relative_directories() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        let agent = |cwd| StartArgs {
            name: "assistant".to_owned(),
            kind: ToolKind::Claude,
            cwd,
        };
        register(&store, agent(root.path().to_owned()), Some(" ".to_owned())).unwrap_err();
        register(&store, agent(".".into()), Some("session".to_owned())).unwrap_err();
        assert!(store.agents().unwrap().is_empty());
    }
}
