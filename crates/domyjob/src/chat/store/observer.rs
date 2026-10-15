use domyjob_core::chat::card::{Card, MachineCard};
use domyjob_core::chat::event::{Body, Stamp};
use domyjob_core::chat::id::{AgentId, AgentName, Conversation, EventId, Origin};
use domyjob_core::chat::notification::{Activity, Delivery};
use domyjob_core::dashboard::MAX_ROWS;
use redb::{ReadableDatabase as _, ReadableTable as _};

use super::tables::{LINKS, MACHINES, META, PEERS, PROFILES, ROOMS, decode, get_text};
use super::{Link, Reader as _, StoreError, views};
use crate::{layout, lock::OsLock, state_io};

pub(crate) const MAX_MESSAGES: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) actor: Option<AgentId>,
    pub(crate) conversation: Option<Conversation>,
    pub(crate) initial_actor: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Machine {
    pub(crate) id: Origin,
    pub(crate) label: String,
    pub(crate) card: MachineCard,
    pub(crate) local: bool,
    pub(crate) link: Option<Link>,
}

#[derive(Debug)]
pub(crate) struct Agent {
    pub(crate) id: AgentId,
    pub(crate) card: Card,
    pub(crate) activity: Option<Activity>,
}

#[derive(Debug)]
pub(crate) struct Chat {
    pub(crate) id: Conversation,
    pub(crate) label: String,
}

#[derive(Debug)]
pub(crate) struct Message {
    pub(crate) id: EventId,
    pub(crate) from: String,
    pub(crate) text: String,
    pub(crate) at: Stamp,
    pub(crate) recipients: Vec<(AgentId, Delivery)>,
}

#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    pub(crate) observed: Option<u64>,
    pub(crate) actor: Option<AgentId>,
    pub(crate) machines: Vec<Machine>,
    pub(crate) agents: Vec<Agent>,
    pub(crate) chats: Vec<Chat>,
    pub(crate) messages: Vec<Message>,
    pub(crate) local_agents: Vec<AgentId>,
}

pub(crate) fn observe(state: &layout::State, request: &Request) -> Result<Snapshot, StoreError> {
    let paths = state.chat();
    let Some(lock_file) = state_io::open_existing_lock(&paths.lock())? else {
        return absent(request);
    };
    let Some(lock) = OsLock::try_file(lock_file)? else {
        return Err(StoreError::Corrupt("chat store busy; retry refresh"));
    };
    let Some(file) = state_io::open_read(&paths.database())? else {
        return absent(request);
    };
    let database = redb::ReadOnlyDatabase::open(paths.database())?;
    let read = database.begin_read()?;
    let meta = read.table(META)?;
    let format = get_text(&meta, "format")?.ok_or(StoreError::Corrupt("chat format"))?;
    if !crate::formats::compatible_chat(&format) {
        return Err(StoreError::Format(format));
    }
    let origin =
        Origin::try_from(get_text(&meta, "origin")?.ok_or(StoreError::Corrupt("chat origin"))?)?;
    let result = snapshot(&read, &origin, request);
    drop(meta);
    drop(read);
    drop(database);
    drop(file);
    drop(lock);
    result
}

fn absent(request: &Request) -> Result<Snapshot, StoreError> {
    if request.actor.is_some() || request.initial_actor.is_some() || request.conversation.is_some()
    {
        return Err(StoreError::Corrupt("the requested viewing state is absent"));
    }
    Ok(Snapshot::default())
}

fn snapshot(
    read: &impl super::Reader,
    origin: &Origin,
    request: &Request,
) -> Result<Snapshot, StoreError> {
    let mut snapshot = Snapshot::default();
    for entry in read.table(PROFILES)?.iter()?.take(MAX_ROWS) {
        let (id, card) = entry?;
        let id = AgentId::try_from(id.value().to_owned())?;
        if id.origin() == origin {
            snapshot.local_agents.push(id.clone());
        }
        snapshot.agents.push(Agent {
            activity: views::activity(read, &id)?,
            id,
            card: decode(card.value())?,
        });
    }
    snapshot.actor = viewing_actor(read, origin, request)?;
    machines(read, origin, &mut snapshot)?;
    if let Some(actor) = snapshot.actor.clone() {
        conversations(read, &actor, &mut snapshot)?;
        if let Some(conversation) = &request.conversation {
            if !snapshot.chats.iter().any(|chat| chat.id == *conversation) {
                return Err(StoreError::Corrupt(
                    "the viewing identity cannot access this conversation",
                ));
            }
            messages(read, conversation, &actor, &mut snapshot)?;
        }
    }
    snapshot.observed = crate::platform::clock::observed_millis();
    Ok(snapshot)
}

fn viewing_actor(
    read: &impl super::Reader,
    origin: &Origin,
    request: &Request,
) -> Result<Option<AgentId>, StoreError> {
    let actor = match (&request.actor, &request.initial_actor) {
        (Some(actor), _) => actor.clone(),
        (None, Some(name)) => {
            if name.contains('@') {
                AgentId::try_from(name.clone())?
            } else {
                AgentId::new(AgentName::try_from(name.clone())?, origin.clone())
            }
        }
        (None, None) => return Ok(None),
    };
    let identity = if actor.origin() == origin {
        domyjob_core::dashboard::IdentityMatch::Exact
    } else {
        domyjob_core::dashboard::IdentityMatch::Different
    };
    if !domyjob_core::dashboard::viewing_identity(identity, views::profile(read, &actor)?.is_some())
    {
        return Err(StoreError::Corrupt(
            "the viewing identity is not a current local agent",
        ));
    }
    Ok(Some(actor))
}

fn machines(
    read: &impl super::Reader,
    origin: &Origin,
    snapshot: &mut Snapshot,
) -> Result<(), StoreError> {
    let peers = read.table(PEERS)?;
    let links = read.table(LINKS)?;
    for entry in read.table(MACHINES)?.iter()?.take(MAX_ROWS) {
        let (id, card) = entry?;
        let id = Origin::try_from(id.value().to_owned())?;
        let card: MachineCard = decode(card.value())?;
        let mut label = card.label.as_str().to_owned();
        let mut link = None;
        for peer in peers.iter()?.take(MAX_ROWS) {
            let (alias, peer_origin) = peer?;
            if peer_origin.value() == id.as_str() {
                alias.value().clone_into(&mut label);
                link = links
                    .get(alias.value())?
                    .map(|value| decode(value.value()))
                    .transpose()?;
                break;
            }
        }
        snapshot.machines.push(Machine {
            local: id == *origin,
            id,
            label,
            card,
            link,
        });
    }
    Ok(())
}

fn conversations(
    read: &impl super::Reader,
    actor: &AgentId,
    snapshot: &mut Snapshot,
) -> Result<(), StoreError> {
    let prefix = format!("{actor}\u{1f}");
    for entry in read
        .table(super::tables::JOINED)?
        .range(prefix.as_str()..)?
        .take(MAX_ROWS)
    {
        let (key, _) = entry?;
        let Some(id) = key.value().strip_prefix(&prefix) else {
            break;
        };
        let id = Conversation::try_from(id.to_owned())?;
        if let Conversation::Direct(pair) = &id
            && let Some(other) = pair.other(actor)
        {
            let label = views::profile(read, other)?.map_or_else(
                || other.name().to_string(),
                |card| card.display_name.as_str().to_owned(),
            );
            snapshot.chats.push(Chat { id, label });
        }
    }
    for entry in read.table(ROOMS)?.iter()?.take(MAX_ROWS) {
        if snapshot.chats.len() >= MAX_ROWS {
            break;
        }
        let (id, room) = entry?;
        let room: domyjob_core::chat::ledger::Room = decode(room.value())?;
        if room.members.contains(actor) {
            snapshot.chats.push(Chat {
                id: Conversation::try_from(id.value().to_owned())?,
                label: room
                    .topic
                    .as_ref()
                    .map_or_else(|| id.value().to_owned(), |topic| topic.as_str().to_owned()),
            });
        }
    }
    Ok(())
}

fn messages(
    read: &impl super::Reader,
    conversation: &Conversation,
    actor: &AgentId,
    snapshot: &mut Snapshot,
) -> Result<(), StoreError> {
    let (first, last) = super::tables::thread_bounds(conversation);
    for entry in read
        .table(super::tables::THREADS)?
        .range(first.as_str()..last.as_str())?
        .rev()
        .take(MAX_MESSAGES)
    {
        let (_, id) = entry?;
        let id = EventId::try_from(id.value().to_owned())?;
        let Some(event) = views::event(read, &id)? else {
            continue;
        };
        if let Body::Message {
            from,
            text,
            at,
            notice,
            audience,
            ..
        } = event.body()
        {
            if event.body().conversation() != Some(conversation)
                || !audience.includes(actor.origin())
            {
                continue;
            }
            let recipients = notice
                .as_ref()
                .map_or_else(Vec::new, |notice| notice.recipients.agents().to_vec())
                .into_iter()
                .map(|recipient| {
                    Ok((
                        recipient.clone(),
                        views::delivery(read, &recipient, event.id())?,
                    ))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            snapshot.messages.push(Message {
                id,
                from: from.to_string(),
                text: text.as_str().to_owned(),
                at: *at,
                recipients,
            });
        }
    }
    snapshot.messages.reverse();
    Ok(())
}

#[cfg(test)]
mod tests;
