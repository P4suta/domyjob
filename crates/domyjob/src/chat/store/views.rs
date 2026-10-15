use std::collections::BTreeMap;

use domyjob_core::chat::card::{Card, MachineCard};
use domyjob_core::chat::event::{Body, Digest, Event};
use domyjob_core::chat::id::{AgentId, AgentName, Conversation, EventId, Origin, RoomId};
use domyjob_core::chat::ledger::{Cursor, Resolution, Room};
use domyjob_core::chat::notification::{
    Activity, ActivityState, Delivery, MAX_STATUS_BYTES, STALLED_MILLIS, activity_state,
};
use domyjob_core::chat::policy::Usage;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use super::StoreError;
use super::tables::{
    ACTIVITY, COUNTERS, CURSORS, DELIVERY, EVENTS, JOINED, LOCAL_AGENTS, MACHINES, OPEN, PROFILES,
    READS, RESOLUTIONS, ROOMS, Reader, STARTED, THREADS, TOMBSTONES, decode, delivery_key,
    event_key, get_json, get_number, get_text, order_key, room_key, thread_bounds, thread_key,
};

#[derive(Debug)]
pub(crate) enum Stored {
    Event { event: Box<Event>, encoded: String },
    Cleaned { clock: u64, digest: Digest },
}

pub(crate) fn stored(reader: &impl Reader, id: &EventId) -> Result<Option<Stored>, StoreError> {
    let key = event_key(id);
    if let Some(encoded) = reader.table(EVENTS)?.get(key.as_str())? {
        let encoded = encoded.value().to_owned();
        let event: Event = decode(&encoded)?;
        if event.id() != id {
            return Err(StoreError::Corrupt("event identity"));
        }
        return Ok(Some(Stored::Event {
            event: Box::new(event),
            encoded,
        }));
    }
    let tombstones = reader.table(TOMBSTONES)?;
    let Some(cleaned) = tombstones.get(key.as_str())? else {
        return Ok(None);
    };
    let (clock, digest) = cleaned.value();
    let digest =
        <[u8; 32]>::try_from(digest).map_err(|_length| StoreError::Corrupt("tombstone"))?;
    Ok(Some(Stored::Cleaned {
        clock,
        digest: Digest::from_bytes(digest),
    }))
}

pub(crate) fn event(reader: &impl Reader, id: &EventId) -> Result<Option<Event>, StoreError> {
    Ok(match stored(reader, id)? {
        Some(Stored::Event { event, .. }) => Some(*event),
        Some(Stored::Cleaned { .. }) | None => None,
    })
}

pub(crate) fn cursor(reader: &impl Reader, origin: &Origin) -> Result<Cursor, StoreError> {
    Ok(reader
        .table(CURSORS)?
        .get(origin.as_str())?
        .map(|value| {
            let (seen, clock) = value.value();
            Cursor { seen, clock }
        })
        .unwrap_or_default())
}

pub(crate) fn resolution(
    reader: &impl Reader,
    request: &EventId,
) -> Result<Option<Resolution>, StoreError> {
    get_json(&reader.table(RESOLUTIONS)?, &event_key(request))
}

pub(crate) fn delivery(
    reader: &impl Reader,
    agent: &AgentId,
    request: &EventId,
) -> Result<Delivery, StoreError> {
    Ok(get_json(&reader.table(DELIVERY)?, &delivery_key(agent, request))?.unwrap_or_default())
}

pub(crate) fn activity(
    reader: &impl Reader,
    agent: &AgentId,
) -> Result<Option<Activity>, StoreError> {
    get_json(&reader.table(ACTIVITY)?, &agent.to_string())
}

pub(crate) fn profile(reader: &impl Reader, agent: &AgentId) -> Result<Option<Card>, StoreError> {
    get_json(&reader.table(PROFILES)?, &agent.to_string())
}

pub(crate) fn room(reader: &impl Reader, room: &RoomId) -> Result<Option<Room>, StoreError> {
    get_json(&reader.table(ROOMS)?, &room_key(room))
}

pub(crate) fn usage(reader: &impl Reader) -> Result<Usage, StoreError> {
    let counters = reader.table(COUNTERS)?;
    Ok(Usage {
        events: get_number(&counters, "events")?,
        bytes: get_number(&counters, "bytes")?,
    })
}

fn entries<T: for<'de> Deserialize<'de>>(
    table: &impl ReadableTable<&'static str, &'static str>,
) -> Result<Vec<(String, T)>, StoreError> {
    let mut found = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        found.push((key.value().to_owned(), decode(value.value())?));
    }
    Ok(found)
}

fn parse_entries<K: TryFrom<String>, T: for<'de> Deserialize<'de>>(
    table: &impl ReadableTable<&'static str, &'static str>,
) -> Result<Vec<(K, T)>, StoreError> {
    entries(table)?
        .into_iter()
        .map(|(key, value)| {
            K::try_from(key)
                .map(|key| (key, value))
                .map_err(|_invalid| StoreError::Corrupt("stored chat key"))
        })
        .collect()
}

pub(crate) fn rooms(reader: &impl Reader) -> Result<Vec<(Conversation, Room)>, StoreError> {
    parse_entries(&reader.table(ROOMS)?)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalAgent {
    pub(crate) cwd: String,
    #[serde(default)]
    pub(crate) session: Option<String>,
}

pub(crate) fn agent_config(
    reader: &impl Reader,
    name: &AgentName,
) -> Result<Option<LocalAgent>, StoreError> {
    get_json(&reader.table(LOCAL_AGENTS)?, name.as_str())
}

pub(crate) fn local_agents(
    reader: &impl Reader,
) -> Result<Vec<(AgentName, LocalAgent)>, StoreError> {
    parse_entries(&reader.table(LOCAL_AGENTS)?)
}

pub(crate) fn open_asks(reader: &impl Reader) -> Result<Vec<(EventId, AgentId)>, StoreError> {
    let mut found = Vec::new();
    for entry in reader.table(OPEN)?.iter()? {
        let (request, responder) = entry?;
        let parse = |text: &str| text.to_owned();
        found.push((
            EventId::try_from(parse(request.value()))?,
            AgentId::try_from(parse(responder.value()))?,
        ));
    }
    Ok(found)
}

fn event_at(reader: &impl Reader, id: &str) -> Result<Option<Event>, StoreError> {
    event(reader, &EventId::try_from(id.to_owned())?)
}

pub(crate) fn thread(
    reader: &impl Reader,
    conversation: &Conversation,
    limit: usize,
    before: Option<&Event>,
) -> Result<Vec<Event>, StoreError> {
    let (first, last) = thread_bounds(conversation);
    let end = before.map_or(last, |event| thread_key(conversation, &order_key(event)));
    let table = reader.table(THREADS)?;
    let mut events = Vec::new();
    for entry in table.range(first.as_str()..end.as_str())?.rev() {
        if events.len() >= limit {
            break;
        }
        let (_, id) = entry?;
        if let Some(event) = event_at(reader, id.value())? {
            events.push(event);
        }
    }
    events.reverse();
    Ok(events)
}

fn conversations_of(
    reader: &impl Reader,
    agent: &AgentId,
) -> Result<Vec<Conversation>, StoreError> {
    let prefix = format!("{agent}\u{1f}");
    let mut found = Vec::new();
    for entry in reader.table(JOINED)?.range(prefix.as_str()..)? {
        let (key, _) = entry?;
        let Some(conversation) = key.value().strip_prefix(prefix.as_str()) else {
            break;
        };
        found.push(Conversation::try_from(conversation.to_owned())?);
    }
    for (conversation, room) in rooms(reader)? {
        if room.members.contains(agent) {
            found.push(conversation);
        }
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

#[derive(Debug, Clone, Copy)]
enum InboxMode {
    All,
    Unread,
    Pending,
}

#[derive(Debug, Clone, Copy)]
struct InboxQuery<'a> {
    after: &'a str,
    limit: usize,
    mode: InboxMode,
}

pub(crate) fn inbox(
    reader: &impl Reader,
    agent: &AgentId,
    after: &str,
    limit: usize,
) -> Result<Vec<Event>, StoreError> {
    find_messages(
        reader,
        agent,
        InboxQuery {
            after,
            limit,
            mode: InboxMode::All,
        },
    )
}

fn find_messages(
    reader: &impl Reader,
    agent: &AgentId,
    query: InboxQuery<'_>,
) -> Result<Vec<Event>, StoreError> {
    let InboxQuery { after, limit, mode } = query;
    let table = reader.table(THREADS)?;
    let mut found: Vec<(String, String)> = Vec::new();
    for conversation in conversations_of(reader, agent)? {
        let (_, last) = thread_bounds(&conversation);
        let start = thread_key(&conversation, after);
        for entry in table.range(start.as_str()..last.as_str())? {
            let (key, id) = entry?;
            let order = key
                .value()
                .rsplit_once('\u{1f}')
                .map(|(_, order)| order.to_owned())
                .ok_or(StoreError::Corrupt("thread key"))?;
            if order.as_str() > after {
                found.push((order, id.value().to_owned()));
            }
        }
    }
    found.sort();
    let mut events = Vec::new();
    for (_, id) in found {
        if events.len() >= limit {
            break;
        }
        let Some(event) = event_at(reader, &id)? else {
            continue;
        };
        let addressed = matches!(event.body(), Body::Message { notice, .. }
            if event.author().as_ref() != Some(agent)
            && notice.as_ref().is_none_or(|notice| notice.recipients.contains(agent)))
            || matches!(event.body(), Body::Resolved { .. })
                && event.author().as_ref() != Some(agent);
        let selected = match mode {
            InboxMode::All => true,
            InboxMode::Unread => {
                if matches!(event.body(), Body::Message { .. }) {
                    delivery(reader, agent, event.id())? < Delivery::Read
                } else {
                    order_key(&event)
                        > get_text(&reader.table(READS)?, &agent.to_string())?.unwrap_or_default()
                }
            }
            InboxMode::Pending => {
                matches!(event.body(), Body::Message { .. })
                    && delivery(reader, agent, event.id())? == Delivery::Pending
            }
        };
        if addressed && selected {
            events.push(event);
        }
    }
    Ok(events)
}

pub(crate) fn unread(
    reader: &impl Reader,
    agent: &AgentId,
    limit: usize,
) -> Result<Vec<Event>, StoreError> {
    selected_inbox(reader, agent, limit, InboxMode::Unread)
}

pub(crate) fn pending(
    reader: &impl Reader,
    agent: &AgentId,
    limit: usize,
) -> Result<Vec<Event>, StoreError> {
    selected_inbox(reader, agent, limit, InboxMode::Pending)
}

fn selected_inbox(
    reader: &impl Reader,
    agent: &AgentId,
    limit: usize,
    mode: InboxMode,
) -> Result<Vec<Event>, StoreError> {
    find_messages(
        reader,
        agent,
        InboxQuery {
            after: "",
            limit,
            mode,
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Presence {
    pub(crate) queued: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) working_on: Option<EventId>,
    pub(crate) online: bool,
    pub(crate) state: ActivityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_call: Option<domyjob_core::chat::event::Stamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<domyjob_core::chat::id::Line<MAX_STATUS_BYTES>>,
}

impl Default for Presence {
    fn default() -> Self {
        Self {
            queued: 0,
            working_on: None,
            online: false,
            state: ActivityState::Stalled,
            latest_call: None,
            status: None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Directory {
    pub(crate) agents: BTreeMap<AgentId, (Card, Presence)>,
    pub(crate) machines: BTreeMap<Origin, MachineCard>,
}

pub(crate) fn directory(reader: &impl Reader) -> Result<Directory, StoreError> {
    let mut directory = Directory::default();
    for (agent, card) in parse_entries::<AgentId, Card>(&reader.table(PROFILES)?)? {
        let current = activity(reader, &agent)?;
        let state = current.as_ref().map_or(ActivityState::Stalled, |activity| {
            activity_state(
                activity.state,
                activity.at.millis(),
                crate::platform::clock::stamp().millis(),
                STALLED_MILLIS,
            )
        });
        let presence = Presence {
            online: state != ActivityState::Stalled,
            state,
            latest_call: current.as_ref().map(|activity| activity.at),
            status: current.and_then(|activity| activity.status),
            ..Presence::default()
        };
        directory.agents.insert(agent, (card, presence));
    }
    for (_, responder) in open_asks(reader)? {
        if let Some((_, presence)) = directory.agents.get_mut(&responder) {
            presence.queued = presence.queued.saturating_add(1);
        }
    }
    for entry in reader.table(STARTED)?.iter()? {
        let (request, agent) = entry?;
        let agent = AgentId::try_from(agent.value().to_owned())?;
        if let Some((_, presence)) = directory.agents.get_mut(&agent) {
            presence.working_on = Some(EventId::try_from(request.value().to_owned())?);
        }
    }
    directory.machines = parse_entries(&reader.table(MACHINES)?)?
        .into_iter()
        .collect();
    Ok(directory)
}
