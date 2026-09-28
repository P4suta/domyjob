//! What chat operations return, as JSON for tools and as text for people.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use domyjob_core::chat::card::{Access, Card, Mode, Os, Relevance, Tag};
use domyjob_core::chat::event::{Body, Event, Intent};
use domyjob_core::chat::id::{AgentId, Conversation, EventId};
use domyjob_core::chat::ledger::{Ending, Resolution, Room};
use domyjob_core::domain::terminal_text;
use serde::Serialize;
use serde_json::{Value, json};

use super::address::Book;
pub(crate) use super::store::Presence;
use super::store::{Directory, Link, LinkState};
use crate::platform::clock;

/// One agent as a directory lists it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgentView {
    pub(crate) agent: String,
    pub(crate) id: AgentId,
    pub(crate) machine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) os: Option<Os>,
    pub(crate) card: Card,
    pub(crate) presence: Presence,
    pub(crate) reachability: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RoomView {
    pub(crate) room: String,
    pub(crate) id: Conversation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) topic: Option<String>,
    pub(crate) members: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DirectoryView {
    pub(crate) agents: Vec<AgentView>,
    pub(crate) rooms: Vec<RoomView>,
}

/// One event of a conversation in reading form.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MessageView {
    pub(crate) id: EventId,
    pub(crate) conversation: Conversation,
    pub(crate) from: String,
    pub(crate) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) responder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) request: Option<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) at: Option<u64>,
}

/// Where an ask stands.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AskState {
    pub(crate) message_id: EventId,
    pub(crate) state: &'static str,
    pub(crate) responder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) answer: Option<MessageView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ended_by: Option<EventId>,
}

impl AskState {
    #[must_use]
    pub(crate) fn ended(&self) -> bool {
        !matches!(self.state, "pending" | "working")
    }

    /// 0 when answered, 3 while pending or working, 1 for any other ending.
    #[must_use]
    pub(crate) fn exit_code(&self) -> u8 {
        match self.state {
            "answered" => 0,
            "pending" | "working" => 3,
            _ => 1,
        }
    }
}

/// The result of one chat operation.
#[derive(Debug)]
pub(crate) enum Outcome {
    Agent {
        agent: AgentId,
        card: Card,
        presence: Presence,
    },
    Removed {
        agent: AgentId,
        ended: usize,
    },
    Directory(DirectoryView),
    Rooms(Vec<RoomView>),
    Posted(Event),
    Asked(AskState),
    Events {
        events: Vec<MessageView>,
        unread: Option<usize>,
    },
}

fn author_label(book: &Book, event: &Event) -> String {
    event.author().map_or_else(
        || book.machine_label(event.origin()),
        |agent| book.agent_label(&agent),
    )
}

fn view(book: &Book, event: &Event) -> Option<MessageView> {
    let conversation = event.body().conversation()?.clone();
    let (kind, text, responder, request, at) = match event.body() {
        Body::Message {
            text, intent, at, ..
        } => {
            let (kind, responder, request) = match intent {
                Intent::Send {} => ("send", None, None),
                Intent::Ask { responder, .. } => ("ask", Some(book.agent_label(responder)), None),
                Intent::Reply { request } => ("reply", None, Some(request.clone())),
            };
            (
                kind,
                Some(text.as_str().to_owned()),
                responder,
                request,
                Some(*at),
            )
        }
        Body::TurnStarted { request, .. } => ("working", None, None, Some(request.clone()), None),
        Body::Resolved {
            request, outcome, ..
        } => (outcome.as_str(), None, None, Some(request.clone()), None),
        Body::Profile { .. }
        | Body::Left { .. }
        | Body::Machine { .. }
        | Body::Room { .. }
        | Body::RoomClosed { .. }
        | Body::Omitted {} => return None,
    };
    Some(MessageView {
        id: event.id().clone(),
        conversation,
        from: author_label(book, event),
        kind,
        text,
        responder,
        request,
        at,
    })
}

pub(crate) fn messages(book: &Book, events: &[Event]) -> Vec<MessageView> {
    events
        .iter()
        .filter_map(|event| view(book, event))
        .collect()
}

/// What this machine knows about one ask.
#[derive(Debug)]
pub(crate) struct AskFacts<'a> {
    pub(crate) request: &'a Event,
    pub(crate) resolution: Option<&'a Resolution>,
    pub(crate) answer: Option<&'a Event>,
    pub(crate) working: bool,
}

pub(crate) fn ask_state(book: &Book, facts: &AskFacts<'_>) -> AskState {
    let AskFacts {
        request,
        resolution,
        answer,
        working,
    } = *facts;
    let responder = match request.body() {
        Body::Message {
            intent: Intent::Ask { responder, .. },
            ..
        } => book.agent_label(responder),
        Body::Message { .. }
        | Body::Profile { .. }
        | Body::Left { .. }
        | Body::Machine { .. }
        | Body::Room { .. }
        | Body::RoomClosed { .. }
        | Body::TurnStarted { .. }
        | Body::Resolved { .. }
        | Body::Omitted {} => String::new(),
    };
    let state = match (resolution.map(|resolution| resolution.ending), working) {
        (Some(Ending::Answered), _) => "answered",
        (Some(Ending::Ended(outcome)), _) => outcome.as_str(),
        (None, true) => "working",
        (None, false) => "pending",
    };
    AskState {
        message_id: request.id().clone(),
        state,
        responder,
        answer: answer.and_then(|event| view(book, event)),
        ended_by: resolution.map(|resolution| resolution.event.clone()),
    }
}

fn room_view(book: &Book, id: Conversation, room: &Room) -> RoomView {
    let label = match &id {
        Conversation::Room(owned) => {
            format!("{}@{}", owned.name(), book.machine_label(owned.origin()))
        }
        Conversation::Direct(_) => id.to_string(),
    };
    RoomView {
        room: label,
        id,
        topic: room.topic.as_ref().map(|topic| topic.as_str().to_owned()),
        members: room
            .members
            .agents()
            .iter()
            .map(|agent| book.agent_label(agent))
            .collect(),
    }
}

pub(crate) fn rooms(book: &Book, rooms: Vec<(Conversation, Room)>) -> Vec<RoomView> {
    rooms
        .into_iter()
        .map(|(id, room)| room_view(book, id, &room))
        .collect()
}

fn age(at: u64) -> String {
    let seconds = clock::now_millis().saturating_sub(at) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn reachability(book: &Book, links: &BTreeMap<String, Link>, agent: &AgentId) -> String {
    if book.local.as_ref() == Some(agent.origin()) {
        return "this machine".to_owned();
    }
    let link = book
        .peers
        .iter()
        .find(|(_, origin)| *origin == agent.origin())
        .and_then(|(alias, _)| links.get(alias));
    match link {
        Some(link) if link.state == LinkState::Failed => {
            format!("unreachable since {}", age(link.at))
        }
        Some(link) => format!("reached {}", age(link.at)),
        None => "no direct link from here".to_owned(),
    }
}

/// The directory, ranked for `query`, most relevant and then alphabetical first.
pub(crate) fn directory(
    (book, links): (&Book, &BTreeMap<String, Link>),
    directory: Directory,
    rooms: Vec<(Conversation, Room)>,
    query: &str,
) -> DirectoryView {
    let mut ranked: Vec<(Relevance, AgentView)> = directory
        .agents
        .into_iter()
        .filter_map(|(agent, (card, presence))| {
            let relevance = card.relevance(agent.name(), query)?;
            Some((
                relevance,
                AgentView {
                    agent: book.agent_label(&agent),
                    machine: book.machine_label(agent.origin()),
                    os: directory
                        .machines
                        .get(agent.origin())
                        .map(|machine| machine.os),
                    reachability: reachability(book, links, &agent),
                    id: agent,
                    card,
                    presence,
                },
            ))
        })
        .collect();
    ranked.sort_by(|first, second| (first.0, &first.1.agent).cmp(&(second.0, &second.1.agent)));
    let query = query.trim().to_lowercase();
    DirectoryView {
        agents: ranked.into_iter().map(|(_, agent)| agent).collect(),
        rooms: self::rooms(book, rooms)
            .into_iter()
            .filter(|room| {
                query.is_empty()
                    || room.room.to_lowercase().contains(&query)
                    || room
                        .topic
                        .as_ref()
                        .is_some_and(|topic| topic.to_lowercase().contains(&query))
            })
            .collect(),
    }
}

fn append(text: &mut String, piece: fmt::Arguments<'_>) {
    let _infallible = text.write_fmt(piece);
}

fn safe(text: &str) -> String {
    terminal_text(text)
}

fn describe_card(card: &Card, presence: &Presence) -> String {
    let mut text = safe(card.display_name.as_str());
    if let Some(role) = &card.role {
        append(&mut text, format_args!(" — {}", safe(role.as_str())));
    }
    let mode = match card.mode {
        Mode::Managed => "managed",
        Mode::Interactive => "interactive",
    };
    let access = match card.access {
        Access::Read => "read-only",
        Access::Write => "can write",
    };
    append(
        &mut text,
        format_args!(" [{} · {mode} · {access}]", card.tool.as_str()),
    );
    let skills: Vec<&str> = card.skills.tags().iter().map(Tag::as_str).collect();
    if !skills.is_empty() {
        append(
            &mut text,
            format_args!("\n    skills: {}", skills.join(", ")),
        );
    }
    if let Some(project) = &card.project {
        append(
            &mut text,
            format_args!("\n    project: {}", safe(project.as_str())),
        );
    }
    if let Some(description) = &card.description {
        append(
            &mut text,
            format_args!(
                "\n    {}",
                safe(description.as_str()).replace('\n', "\n    ")
            ),
        );
    }
    if let Some(status) = &card.status {
        append(
            &mut text,
            format_args!("\n    status: {}", safe(status.as_str())),
        );
    }
    match (&presence.working_on, presence.queued) {
        (Some(request), queued) => {
            append(
                &mut text,
                format_args!("\n    working on {request} ({queued} waiting)"),
            );
        }
        (None, 0) => {}
        (None, queued) => {
            append(&mut text, format_args!("\n    {queued} waiting"));
        }
    }
    text
}

fn message_line(message: &MessageView) -> String {
    let when = message
        .at
        .map_or_else(String::new, |at| format!("[{}] ", age(at)));
    let body = match (&message.text, &message.responder, &message.request) {
        (Some(text), Some(responder), _) => format!("asks {}: {}", safe(responder), safe(text)),
        (Some(text), None, Some(request)) => format!("replies to {request}: {}", safe(text)),
        (Some(text), None, None) => safe(text),
        (None, _, Some(request)) => format!("{} {request}", message.kind),
        (None, _, None) => message.kind.to_owned(),
    };
    format!("{when}{} {}: {body}", message.id, safe(&message.from))
}

impl Outcome {
    /// The structured result for `--json` and MCP.
    #[must_use]
    pub(crate) fn json(&self) -> Value {
        match self {
            Self::Agent {
                agent,
                card,
                presence,
            } => json!({"agent": agent, "card": card, "presence": presence}),
            Self::Removed { agent, ended } => json!({"removed": agent, "ended_asks": ended}),
            Self::Directory(directory) => json!(directory),
            Self::Rooms(rooms) => json!({"rooms": rooms}),
            Self::Posted(event) => json!({"message_id": event.id()}),
            Self::Asked(state) => json!(state),
            Self::Events { events, unread } => json!({"events": events, "unread": unread}),
        }
    }

    /// The same result for a terminal.
    #[must_use]
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Agent {
                agent,
                card,
                presence,
            } => format!("{agent}\n    {}", describe_card(card, presence)),
            Self::Removed { agent, ended } => {
                format!("removed {agent}; ended {ended} waiting asks")
            }
            Self::Directory(directory) => {
                let mut text = String::new();
                for agent in &directory.agents {
                    append(
                        &mut text,
                        format_args!(
                            "{} ({})\n    {}\n",
                            safe(&agent.agent),
                            agent.reachability,
                            describe_card(&agent.card, &agent.presence)
                        ),
                    );
                }
                for room in &directory.rooms {
                    append(&mut text, format_args!("{}\n", room_line(room)));
                }
                text.trim_end().to_owned()
            }
            Self::Rooms(rooms) => rooms.iter().map(room_line).collect::<Vec<_>>().join("\n"),
            Self::Posted(event) => event.id().to_string(),
            Self::Asked(state) => match &state.answer {
                Some(answer) => message_line(answer),
                None => format!("{} {}", state.message_id, state.state),
            },
            Self::Events { events, unread } => {
                let mut lines: Vec<String> = events.iter().map(message_line).collect();
                if let Some(unread) = unread.filter(|count| *count > 0) {
                    lines.push(format!("{unread} more unread"));
                }
                lines.join("\n")
            }
        }
    }

    /// The process exit code of this result.
    #[must_use]
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::Asked(state) => state.exit_code(),
            Self::Agent { .. }
            | Self::Removed { .. }
            | Self::Directory(_)
            | Self::Rooms(_)
            | Self::Posted(_)
            | Self::Events { .. } => 0,
        }
    }
}

fn room_line(room: &RoomView) -> String {
    let topic = room
        .topic
        .as_ref()
        .map_or_else(String::new, |topic| format!(" — {}", safe(topic)));
    format!(
        "#{}{topic}\n    members: {}",
        safe(&room.room),
        room.members
            .iter()
            .map(|member| safe(member))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
