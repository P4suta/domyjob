#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::Serialize;

use crate::lock::OsLock;
use crate::state_io;
pub(crate) use domyjob_core::chat_v0::{
    AgentTool, Audience, BATCH, Event, EventData, MAX_TEXT, MessageMode, TurnFailure, direct,
};
use domyjob_core::chat_v0::{
    ChatValidationError, MAX_EVENT_BYTES, valid_event_id, valid_origin, validate_response,
};

const EVENTS: TableDefinition<'static, &str, &str> = TableDefinition::new("chat_events_v1");
const META: TableDefinition<'static, &str, u64> = TableDefinition::new("chat_meta_v1");
const MAX_EVENTS: u64 = 10_000;
const MAX_STORE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChatError {
    #[error("chat state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("chat identity entropy failed: {0}")]
    Entropy(getrandom::Error),
    #[error(transparent)]
    State(#[from] state_io::StateError),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
    #[error(transparent)]
    Database(#[from] redb::DatabaseError),
    #[error(transparent)]
    Transaction(#[from] redb::TransactionError),
    #[error(transparent)]
    Table(#[from] redb::TableError),
    #[error(transparent)]
    Storage(#[from] redb::StorageError),
    #[error(transparent)]
    Commit(#[from] redb::CommitError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("a chat event is invalid: {0}")]
    Invalid(&'static str),
    #[error("chat event {origin}:{seq} conflicts with an event already stored")]
    Conflict { origin: String, seq: u64 },
    #[error("chat events from {origin} have a gap before {seq}")]
    Gap { origin: String, seq: u64 },
    #[error("no chat agent or room matches {0}")]
    Unknown(String),
    #[error("{0} names more than one chat agent; use name@machine")]
    Ambiguous(String),
    #[error("chat request {0} already has a reply or failure")]
    AlreadyResolved(String),
}

impl From<ChatValidationError> for ChatError {
    fn from(error: ChatValidationError) -> Self {
        Self::Invalid(error.0)
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "bounded stored chat records enter through this strict domain decoder"
)]
fn decode_event(text: &str) -> Result<Event, ChatError> {
    if text.len() > MAX_EVENT_BYTES {
        return Err(ChatError::Invalid("event exceeds the transport limit"));
    }
    Ok(serde_json::from_str(text)?)
}

#[derive(Debug, Clone)]
pub(crate) struct Store {
    root: PathBuf,
    origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RoomView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) owner: String,
    pub(crate) members: BTreeSet<String>,
}

fn event_key(origin: &str, seq: u64) -> String {
    format!("{origin}/{seq:016x}")
}

fn seen_key(origin: &str) -> String {
    format!("seen/{origin}")
}

fn clock_key(origin: &str) -> String {
    format!("clock/{origin}")
}

fn ack_key(peer: &str) -> String {
    format!("ack/{peer}")
}

fn number(table: &impl ReadableTable<&'static str, u64>, key: &str) -> Result<u64, ChatError> {
    Ok(table.get(key)?.map_or(0, |value| value.value()))
}

fn parent_key(id: &str) -> Result<String, ChatError> {
    let (origin, sequence) = id
        .rsplit_once(':')
        .filter(|_| valid_event_id(id))
        .ok_or(ChatError::Invalid("reply request ID"))?;
    let sequence = u64::from_str_radix(sequence, 16)
        .map_err(|_invalid| ChatError::Invalid("reply request ID"))?;
    Ok(event_key(origin, sequence))
}

fn response_request(data: &EventData) -> Option<&str> {
    match data {
        EventData::Message {
            mode: MessageMode::Reply { request },
            ..
        }
        | EventData::TurnFailure { request, .. } => Some(request),
        EventData::Agent { .. }
        | EventData::Room { .. }
        | EventData::Membership { .. }
        | EventData::Message { .. }
        | EventData::Omitted {} => None,
    }
}

fn load_event(
    table: &impl ReadableTable<&'static str, &'static str>,
    id: &str,
) -> Result<Event, ChatError> {
    let key = parent_key(id)?;
    let value = table
        .get(key.as_str())?
        .ok_or_else(|| ChatError::Unknown(id.to_owned()))?;
    decode_event(value.value())
}

fn validate_link(
    table: &impl ReadableTable<&'static str, &'static str>,
    data: &EventData,
    resolution: bool,
    clock: Option<u64>,
) -> Result<(), ChatError> {
    if let Some(request) = response_request(data) {
        let original = load_event(table, request)?;
        validate_response(&original, data, resolution)?;
        if clock.is_some_and(|clock| clock <= original.clock) {
            return Err(ChatError::Invalid("reply clock must follow its request"));
        }
    }
    Ok(())
}

fn encode_event(event: &Event) -> Result<String, ChatError> {
    event.validate()?;
    let payload = serde_json::to_string(event)?;
    if payload.len() > MAX_EVENT_BYTES {
        return Err(ChatError::Invalid("event exceeds the transport limit"));
    }
    Ok(payload)
}

fn record_resolution(
    meta: &mut redb::Table<'_, &str, u64>,
    request: &str,
    sequence: u64,
) -> Result<(), ChatError> {
    let key = format!("resolution/{request}");
    if number(meta, &key)? != 0 {
        return Err(ChatError::AlreadyResolved(request.to_owned()));
    }
    meta.insert(key.as_str(), sequence)?;
    Ok(())
}

fn validate_incoming_link(
    table: &impl ReadableTable<&'static str, &'static str>,
    meta: &mut redb::Table<'_, &str, u64>,
    event: &Event,
) -> Result<(), ChatError> {
    if let Some(request) = response_request(&event.data) {
        let original = load_event(table, request)?;
        let resolution = matches!(
            &original.data,
            EventData::Message {
                mode: MessageMode::Ask { .. },
                ..
            }
        );
        validate_link(table, &event.data, resolution, Some(event.clock))?;
        if resolution {
            record_resolution(meta, request, event.seq)?;
        }
    }
    Ok(())
}

fn record_event(
    table: &mut redb::Table<'_, &str, &str>,
    meta: &mut redb::Table<'_, &str, u64>,
    event: &Event,
    payload: &str,
) -> Result<(), ChatError> {
    let bytes = number(meta, "bytes")?
        .checked_add(
            u64::try_from(payload.len()).map_err(|_size| ChatError::Invalid("event size"))?,
        )
        .ok_or(ChatError::Invalid("chat storage size exhausted"))?;
    if table.len()? >= MAX_EVENTS || bytes > MAX_STORE_BYTES {
        return Err(ChatError::Invalid(
            "chat history reached its 10000-event or 128 MiB limit",
        ));
    }
    table.insert(event_key(&event.origin, event.seq).as_str(), payload)?;
    meta.insert("bytes", bytes)?;
    Ok(())
}

impl Store {
    pub(crate) fn open() -> Result<Self, ChatError> {
        let state = crate::platform::state()?.join("v1");
        let root = state.join("chat");
        state_io::private_dir(&root)?;
        let lock = OsLock::exclusive(&root.join("chat.lock"))?;
        let origin_path = root.join("origin");
        let origin = if let Some(bytes) = state_io::read_bytes(&origin_path)? {
            String::from_utf8(bytes)
                .map_err(|_invalid| ChatError::Invalid("stored chat identity"))?
        } else {
            let mut entropy = [0_u8; 16];
            getrandom::fill(&mut entropy).map_err(ChatError::Entropy)?;
            let origin = data_encoding::HEXLOWER.encode(&entropy);
            state_io::write_bytes(&origin_path, origin.as_bytes())?;
            origin
        };
        if !valid_origin(&origin) {
            return Err(ChatError::Invalid("stored chat identity"));
        }
        drop(lock);
        Ok(Self::at(&state, origin))
    }

    #[must_use]
    pub(crate) fn at(state: &Path, origin: String) -> Self {
        Self {
            root: state.join("chat"),
            origin,
        }
    }

    #[must_use]
    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    fn agent_lock_path(&self, id: &str, prefix: &str) -> Result<PathBuf, ChatError> {
        state_io::private_dir(&self.root)?;
        let hash = blake3::hash(id.as_bytes()).to_hex();
        Ok(self.root.join(format!("{prefix}-{hash}.lock")))
    }

    pub(crate) fn agent_launch_lock(&self, id: &str) -> Result<OsLock, ChatError> {
        Ok(OsLock::exclusive(
            &self.agent_lock_path(id, "launch-agent")?,
        )?)
    }

    pub(crate) fn try_lock_agent(&self, id: &str) -> Result<Option<OsLock>, ChatError> {
        Ok(OsLock::try_exclusive(&self.agent_lock_path(id, "agent")?)?)
    }

    fn with_db<T>(
        &self,
        use_db: impl FnOnce(&Database) -> Result<T, ChatError>,
    ) -> Result<T, ChatError> {
        if !valid_origin(&self.origin) {
            return Err(ChatError::Invalid("machine identity"));
        }
        state_io::private_dir(&self.root)?;
        let lock = OsLock::exclusive(&self.root.join("chat.lock"))?;
        let file = state_io::open_lock(&self.root.join("chat.redb"))?;
        let db = Database::builder().create_file(file)?;
        let result = use_db(&db);
        drop(db);
        drop(lock);
        result
    }

    pub(crate) fn append(&self, data: EventData) -> Result<Event, ChatError> {
        self.append_inner(data, None)
    }

    pub(crate) fn append_resolution(&self, data: EventData) -> Result<Event, ChatError> {
        let request = match &data {
            EventData::Message {
                mode: MessageMode::Reply { request },
                ..
            }
            | EventData::TurnFailure { request, .. } => request.clone(),
            EventData::Agent { .. }
            | EventData::Room { .. }
            | EventData::Membership { .. }
            | EventData::Message { .. }
            | EventData::Omitted {} => {
                return Err(ChatError::Invalid("resolution must reference its request"));
            }
        };
        self.append_inner(data, Some(&request))
    }

    fn append_inner(&self, data: EventData, resolution: Option<&str>) -> Result<Event, ChatError> {
        data.validate(&self.origin)?;
        self.with_db(|db| {
            let write = db.begin_write()?;
            {
                let table = write.open_table(EVENTS)?;
                validate_link(&table, &data, resolution.is_some(), None)?;
            }
            let event = {
                let mut meta = write.open_table(META)?;
                let seq = number(&meta, &seen_key(&self.origin))?
                    .checked_add(1)
                    .ok_or(ChatError::Invalid("local sequence is exhausted"))?;
                let clock = number(&meta, "clock")?
                    .checked_add(1)
                    .filter(|clock| *clock < u64::MAX)
                    .ok_or(ChatError::Invalid("logical clock is exhausted"))?;
                let event = Event {
                    origin: self.origin.clone(),
                    seq,
                    clock,
                    data,
                };
                let payload = encode_event(&event)?;
                record_event(&mut write.open_table(EVENTS)?, &mut meta, &event, &payload)?;
                meta.insert(seen_key(&self.origin).as_str(), seq)?;
                meta.insert("clock", clock)?;
                meta.insert(clock_key(&self.origin).as_str(), clock)?;
                if let Some(request) = resolution {
                    record_resolution(&mut meta, request, seq)?;
                }
                event
            };
            write.commit()?;
            Ok(event)
        })
    }

    pub(crate) fn merge(&self, origin: &str, events: &[Event]) -> Result<u64, ChatError> {
        if events.len() > BATCH || origin == self.origin || !valid_origin(origin) {
            return Err(ChatError::Invalid("sync batch or origin"));
        }
        for event in events {
            if event.origin != origin {
                return Err(ChatError::Invalid("event origin differs from sync origin"));
            }
            event.validate()?;
        }
        self.with_db(|db| {
            let write = db.begin_write()?;
            let seen = {
                let mut meta = write.open_table(META)?;
                let mut table = write.open_table(EVENTS)?;
                let seen_key = seen_key(origin);
                let clock_key = clock_key(origin);
                let mut seen = number(&meta, &seen_key)?;
                let mut clock = number(&meta, "clock")?;
                let mut origin_clock = number(&meta, &clock_key)?;
                for event in events {
                    let key = event_key(origin, event.seq);
                    let payload = encode_event(event)?;
                    if event.seq <= seen {
                        let same = table
                            .get(key.as_str())?
                            .is_some_and(|old| old.value() == payload);
                        if !same {
                            return Err(ChatError::Conflict {
                                origin: origin.to_owned(),
                                seq: event.seq,
                            });
                        }
                    } else if event.seq == seen.saturating_add(1) {
                        if event.clock <= origin_clock {
                            return Err(ChatError::Invalid("origin clock did not advance"));
                        }
                        validate_incoming_link(&table, &mut meta, event)?;
                        record_event(&mut table, &mut meta, event, &payload)?;
                        seen = event.seq;
                        clock = clock.max(event.clock);
                        origin_clock = event.clock;
                    } else {
                        return Err(ChatError::Gap {
                            origin: origin.to_owned(),
                            seq: event.seq,
                        });
                    }
                }
                meta.insert(seen_key.as_str(), seen)?;
                meta.insert("clock", clock)?;
                meta.insert(clock_key.as_str(), origin_clock)?;
                seen
            };
            write.commit()?;
            Ok(seen)
        })
    }

    fn meta_number(&self, key: &str) -> Result<u64, ChatError> {
        self.with_db(|db| {
            let read = db.begin_read()?;
            match read.open_table(META) {
                Ok(meta) => number(&meta, key),
                Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
                Err(error) => Err(error.into()),
            }
        })
    }

    pub(crate) fn seen(&self, origin: &str) -> Result<u64, ChatError> {
        if !valid_origin(origin) {
            return Err(ChatError::Invalid("machine identity"));
        }
        self.meta_number(&seen_key(origin))
    }

    pub(crate) fn ack(&self, peer: &str) -> Result<u64, ChatError> {
        if !valid_origin(peer) {
            return Err(ChatError::Invalid("peer identity"));
        }
        self.meta_number(&ack_key(peer))
    }

    pub(crate) fn record_ack(&self, peer: &str, ack: u64) -> Result<(), ChatError> {
        if !valid_origin(peer) {
            return Err(ChatError::Invalid("peer identity"));
        }
        self.with_db(|db| {
            let write = db.begin_write()?;
            {
                let mut meta = write.open_table(META)?;
                if ack > number(&meta, &seen_key(&self.origin))? {
                    return Err(ChatError::Invalid("acknowledgment exceeds local sequence"));
                }
                let key = ack_key(peer);
                if ack > number(&meta, &key)? {
                    meta.insert(key.as_str(), ack)?;
                }
            }
            write.commit()?;
            Ok(())
        })
    }

    fn read_events<T>(
        &self,
        read_events: impl FnOnce(
            &redb::ReadOnlyTable<&'static str, &'static str>,
        ) -> Result<Vec<T>, ChatError>,
    ) -> Result<Vec<T>, ChatError> {
        self.with_db(|db| {
            let read = db.begin_read()?;
            let table = match read.open_table(EVENTS) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(error) => return Err(error.into()),
            };
            read_events(&table)
        })
    }

    fn scan_events<T>(
        &self,
        mut visit: impl FnMut(&str, &str, &mut Vec<T>) -> Result<bool, ChatError>,
    ) -> Result<Vec<T>, ChatError> {
        self.read_events(|table| {
            let mut found = Vec::new();
            for (count, entry) in table.iter()?.enumerate() {
                if u64::try_from(count)
                    .map_err(|_size| ChatError::Invalid("chat projection size"))?
                    >= MAX_EVENTS
                {
                    return Err(ChatError::Invalid(
                        "chat projection exceeds the event limit",
                    ));
                }
                let (key, value) = entry?;
                if !visit(key.value(), value.value(), &mut found)? {
                    break;
                }
            }
            Ok(found)
        })
    }

    pub(crate) fn after(
        &self,
        origin: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<Event>, ChatError> {
        if !valid_origin(origin) {
            return Err(ChatError::Invalid("machine identity"));
        }
        if limit == 0 || after == u64::MAX {
            return Ok(Vec::new());
        }
        let prefix = format!("{origin}/");
        let start = event_key(
            origin,
            after
                .checked_add(1)
                .ok_or(ChatError::Invalid("sequence is exhausted"))?,
        );
        self.read_events(|table| {
            let mut found = Vec::new();
            for entry in table.range(start.as_str()..)? {
                let (key, value) = entry?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let event: Event = decode_event(value.value())?;
                if event.seq > after {
                    found.push(event);
                }
                if found.len() >= limit.min(BATCH) {
                    break;
                }
            }
            Ok(found)
        })
    }

    pub(crate) fn after_for(
        &self,
        after: u64,
        limit: usize,
        peer: &str,
    ) -> Result<Vec<Event>, ChatError> {
        if !valid_origin(peer) {
            return Err(ChatError::Invalid("peer identity"));
        }
        let mut events = self.after(&self.origin, after, limit)?;
        for event in &mut events {
            match &event.data {
                EventData::Agent {
                    name,
                    tool,
                    managed,
                    ..
                } => {
                    event.data = EventData::Agent {
                        name: name.clone(),
                        tool: *tool,
                        cwd: "<local>".to_owned(),
                        session: None,
                        managed: *managed,
                    };
                }
                EventData::Message { audience, .. } | EventData::TurnFailure { audience, .. }
                    if !audience.includes(peer) =>
                {
                    event.data = EventData::Omitted {};
                }
                EventData::Room { .. }
                | EventData::Membership { .. }
                | EventData::Message { .. }
                | EventData::TurnFailure { .. }
                | EventData::Omitted {} => {}
            }
        }
        Ok(events)
    }

    pub(crate) fn events(&self) -> Result<Vec<Event>, ChatError> {
        let mut events = self.scan_events(|_, value, events| {
            events.push(decode_event(value)?);
            Ok(true)
        })?;
        events.sort_by(|a, b| (a.clock, &a.origin, a.seq).cmp(&(b.clock, &b.origin, b.seq)));
        Ok(events)
    }

    pub(crate) fn agents(&self) -> Result<BTreeMap<String, Event>, ChatError> {
        let mut agents = BTreeMap::new();
        for event in self.events()? {
            if let EventData::Agent { name, .. } = &event.data {
                agents.insert(format!("{name}@{}", event.origin), event.clone());
            }
        }
        Ok(agents)
    }

    pub(crate) fn rooms(&self) -> Result<BTreeMap<String, RoomView>, ChatError> {
        let mut rooms = BTreeMap::new();
        for event in self.events()? {
            match event.data {
                EventData::Room { id, name, members } => {
                    rooms.insert(
                        id.clone(),
                        RoomView {
                            id,
                            name,
                            owner: event.origin,
                            members: members.into_iter().collect(),
                        },
                    );
                }
                EventData::Membership {
                    room,
                    member,
                    present,
                } => {
                    if let Some(current) = rooms.get_mut(&room) {
                        if present {
                            current.members.insert(member);
                        } else {
                            current.members.remove(&member);
                        }
                    }
                }
                EventData::Agent { .. }
                | EventData::Message { .. }
                | EventData::TurnFailure { .. }
                | EventData::Omitted {} => {}
            }
        }
        Ok(rooms)
    }

    pub(crate) fn event(&self, id: &str) -> Result<Option<Event>, ChatError> {
        let key = parent_key(id)?;
        self.with_db(|db| {
            let read = db.begin_read()?;
            let table = match read.open_table(EVENTS) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            table
                .get(key.as_str())?
                .map(|value| decode_event(value.value()))
                .transpose()
        })
    }

    pub(crate) fn mark_notification(&self, id: &str) -> Result<bool, ChatError> {
        self.with_db(|db| {
            let write = db.begin_write()?;
            let event = load_event(&write.open_table(EVENTS)?, id)?;
            if !matches!(&event.data, EventData::Message { audience, .. } if audience.includes(&self.origin)) {
                return Err(ChatError::Invalid("notification must reference a message addressed to this machine"));
            }
            let first = {
                let mut meta = write.open_table(META)?;
                let key = format!("notification/{id}");
                let first = number(&meta, &key)? == 0;
                if first {
                    meta.insert(key.as_str(), 1)?;
                }
                first
            };
            write.commit()?;
            Ok(first)
        })
    }

    pub(crate) fn claim_turn(&self, id: &str) -> Result<bool, ChatError> {
        self.with_db(|db| {
            let write = db.begin_write()?;
            let event = load_event(&write.open_table(EVENTS)?, id)?;
            if !matches!(&event.data, EventData::Message { mode: MessageMode::Ask { responder }, .. } if responder.rsplit_once('@').is_some_and(|(_, origin)| origin == self.origin)) {
                return Err(ChatError::Invalid("turn must be an ask addressed to this machine"));
            }
            let claimed = {
                let mut meta = write.open_table(META)?;
                let key = format!("turn/{id}");
                if number(&meta, &key)? != 0 || number(&meta, &format!("resolution/{id}"))? != 0 {
                    false
                } else {
                    meta.insert(key.as_str(), 1)?;
                    true
                }
            };
            write.commit()?;
            Ok(claimed)
        })
    }

    pub(crate) fn finish_turn(&self, id: &str) -> Result<(), ChatError> {
        self.with_db(|db| {
            let write = db.begin_write()?;
            {
                let mut meta = write.open_table(META)?;
                let key = format!("turn/{id}");
                if number(&meta, &key)? == 0 {
                    return Err(ChatError::Invalid("turn was not claimed"));
                }
                meta.insert(key.as_str(), 2)?;
            }
            write.commit()?;
            Ok(())
        })
    }

    pub(crate) fn pending_turns(&self) -> Result<Vec<String>, ChatError> {
        self.with_db(|db| {
            let read = db.begin_read()?;
            let table = match read.open_table(META) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(error) => return Err(error.into()),
            };
            let mut pending = Vec::new();
            for entry in table.range("turn/".."turn0")? {
                let (key, value) = entry?;
                if let Some(id) = key.value().strip_prefix("turn/")
                    && value.value() == 1
                {
                    pending.push(id.to_owned());
                }
            }
            Ok(pending)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_room_ids_are_symmetric_and_unambiguous() {
        assert_eq!(direct("a:b", "c"), direct("c", "a:b"));
        assert_ne!(direct("a:b", "c"), direct("a", "b:c"));
    }

    #[test]
    fn metadata_names_and_conversation_ids_reject_controls_and_malformed_values() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        assert!(matches!(
            store.append(EventData::Agent {
                name: "bad\u{1b}name".to_owned(),
                tool: AgentTool::Claude,
                cwd: "/project".to_owned(),
                session: None,
                managed: true,
            }),
            Err(ChatError::Invalid(_))
        ));
        assert!(matches!(
            store.append(EventData::Room {
                id: "room:a:bad\nname".to_owned(),
                name: "bad\nname".to_owned(),
                members: vec!["owner@a".to_owned()],
            }),
            Err(ChatError::Invalid(_))
        ));
    }

    fn synchronize(a: &Store, b: &Store) {
        for _ in 0..64 {
            let outgoing_a = a
                .after_for(a.ack(b.origin()).unwrap(), BATCH, b.origin())
                .unwrap();
            let outgoing_b = b
                .after_for(b.ack(a.origin()).unwrap(), BATCH, a.origin())
                .unwrap();
            let seen_a = b.merge(a.origin(), &outgoing_a).unwrap();
            let seen_b = a.merge(b.origin(), &outgoing_b).unwrap();
            a.record_ack(b.origin(), seen_a).unwrap();
            b.record_ack(a.origin(), seen_b).unwrap();
            if outgoing_a.len() < BATCH && outgoing_b.len() < BATCH {
                return;
            }
        }
        panic!("the simulated sync did not converge");
    }

    fn message(from: &str, text: &str) -> EventData {
        let origin = from.rsplit_once('@').unwrap().1.to_owned();
        EventData::Message {
            to: direct(from, "bob@b"),
            from: from.to_owned(),
            text: text.to_owned(),
            audience: vec![origin].try_into().unwrap(),
            mode: MessageMode::Send,
        }
    }

    #[test]
    fn offline_events_merge_once_and_keep_a_total_order() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(&root.path().join("a"), "a".to_owned());
        let b = Store::at(&root.path().join("b"), "b".to_owned());
        let first = a.append(message("alice@a", "from a")).unwrap();
        let second = b.append(message("bob@b", "from b")).unwrap();
        assert_eq!(a.merge("b", std::slice::from_ref(&second)).unwrap(), 1);
        assert_eq!(b.merge("a", std::slice::from_ref(&first)).unwrap(), 1);
        assert_eq!(a.merge("b", &[second]).unwrap(), 1);
        assert_eq!(a.events().unwrap(), b.events().unwrap());
    }

    #[test]
    fn a_gap_or_conflicting_duplicate_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(root.path(), "a".to_owned());
        let event = Event {
            origin: "b".to_owned(),
            seq: 2,
            clock: 1,
            data: message("bob@b", "hello"),
        };
        assert!(matches!(a.merge("b", &[event]), Err(ChatError::Gap { .. })));
        let first = Event {
            origin: "b".to_owned(),
            seq: 1,
            clock: 1,
            data: message("bob@b", "hello"),
        };
        a.merge("b", std::slice::from_ref(&first)).unwrap();
        let altered = Event {
            data: message("bob@b", "different"),
            ..first
        };
        assert!(matches!(
            a.merge("b", &[altered]),
            Err(ChatError::Conflict { .. })
        ));
    }

    #[test]
    fn logical_clocks_cannot_regress_or_overflow() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        let events = [
            Event {
                origin: "b".to_owned(),
                seq: 1,
                clock: 2,
                data: message("bob@b", "first"),
            },
            Event {
                origin: "b".to_owned(),
                seq: 2,
                clock: 1,
                data: message("bob@b", "second"),
            },
        ];
        assert!(matches!(
            store.merge("b", &events),
            Err(ChatError::Invalid("origin clock did not advance"))
        ));
        assert_eq!(store.seen("b").unwrap(), 0);
        store
            .with_db(|db| {
                let write = db.begin_write()?;
                write.open_table(META)?.insert("clock", u64::MAX)?;
                write.commit()?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            store.append(message("alice@a", "cannot wrap")),
            Err(ChatError::Invalid("logical clock is exhausted"))
        ));
        assert!(store.events().unwrap().is_empty());
    }

    #[test]
    fn a_restart_preserves_delivery_and_room_membership() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("a");
        let first = Store::at(&state, "a".to_owned());
        first
            .append(EventData::Room {
                id: "room:a:review".to_owned(),
                name: "review".to_owned(),
                members: vec!["owner@a".to_owned(), "bob@b".to_owned()],
            })
            .unwrap();
        first
            .append(EventData::Membership {
                room: "room:a:review".to_owned(),
                member: "bob@b".to_owned(),
                present: false,
            })
            .unwrap();
        first.record_ack("b", 1).unwrap();
        drop(first);

        let restarted = Store::at(&state, "a".to_owned());
        assert_eq!(restarted.ack("b").unwrap(), 1);
        assert_eq!(restarted.after("a", 1, BATCH).unwrap().len(), 1);
        assert!(
            !restarted
                .rooms()
                .unwrap()
                .get("room:a:review")
                .unwrap()
                .members
                .contains("bob@b")
        );
        assert_eq!(
            restarted.append(message("owner@a", "hello")).unwrap().seq,
            3
        );
    }

    #[test]
    fn a_remote_event_cannot_claim_a_different_sender() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(root.path(), "a".to_owned());
        let forged = Event {
            origin: "b".to_owned(),
            seq: 1,
            clock: 1,
            data: message("owner@a", "forged"),
        };
        assert!(matches!(
            a.merge("b", &[forged]),
            Err(ChatError::Invalid(_))
        ));
    }

    #[test]
    fn agent_registration_sync_hides_local_directory_and_session() {
        let root = tempfile::tempdir().unwrap();
        let local = Store::at(&root.path().join("a"), "a".to_owned());
        let remote = Store::at(&root.path().join("b"), "b".to_owned());
        local
            .append(EventData::Agent {
                name: "reviewer".to_owned(),
                tool: AgentTool::Codex,
                cwd: "/private/project".to_owned(),
                session: Some("secret-session".to_owned()),
                managed: true,
            })
            .unwrap();
        let delivered = local.after_for(0, BATCH, "b").unwrap();
        assert!(matches!(
            delivered.first().map(|event| &event.data),
            Some(EventData::Agent { cwd, session: None, .. }) if cwd == "<local>"
        ));
        remote.merge("a", &delivered).unwrap();
        assert_eq!(remote.agents().unwrap().len(), 1);
        assert!(matches!(
            local.agents().unwrap().get("reviewer@a").map(|event| &event.data),
            Some(EventData::Agent { cwd, session: Some(session), .. })
                if cwd == "/private/project" && session == "secret-session"
        ));
    }

    #[test]
    fn a_machine_outside_the_audience_only_stores_an_empty_event() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(&root.path().join("a"), "a".to_owned());
        let b = Store::at(&root.path().join("b"), "b".to_owned());
        let c = Store::at(&root.path().join("c"), "c".to_owned());
        a.append(EventData::Message {
            to: direct("alice@a", "bob@b"),
            from: "alice@a".to_owned(),
            text: "private text".to_owned(),
            audience: vec!["a".to_owned(), "b".to_owned()].try_into().unwrap(),
            mode: MessageMode::Send,
        })
        .unwrap();
        let for_b = a.after_for(0, BATCH, "b").unwrap();
        let for_c = a.after_for(0, BATCH, "c").unwrap();
        assert!(
            matches!(for_b.first().map(|event| &event.data), Some(EventData::Message { text, .. }) if text == "private text")
        );
        assert!(matches!(
            for_c.first().map(|event| &event.data),
            Some(EventData::Omitted {})
        ));
        b.merge("a", &for_b).unwrap();
        c.merge("a", &for_c).unwrap();
        assert_eq!(
            b.events().unwrap().first().unwrap().id(),
            c.events().unwrap().first().unwrap().id()
        );
        assert!(matches!(
            c.events().unwrap().first().map(|event| &event.data),
            Some(EventData::Omitted {})
        ));
        assert_eq!(c.merge("a", &for_c).unwrap(), 1);
    }

    fn question_and_answer(store: &Store) -> EventData {
        let thread_id = direct("alice@a", "bob@a");
        let request = store
            .append(EventData::Message {
                to: thread_id.clone(),
                from: "alice@a".to_owned(),
                text: "question".to_owned(),
                audience: vec!["a".to_owned()].try_into().unwrap(),
                mode: MessageMode::Ask {
                    responder: "bob@a".to_owned(),
                },
            })
            .unwrap();
        EventData::Message {
            to: thread_id,
            from: "bob@a".to_owned(),
            text: "answer".to_owned(),
            audience: vec!["a".to_owned()].try_into().unwrap(),
            mode: MessageMode::Reply {
                request: request.id(),
            },
        }
    }

    #[test]
    fn concurrent_resolutions_commit_only_one_answer() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        let response = question_and_answer(&store);
        let first = store.clone();
        let second = store.clone();
        let left = std::thread::spawn({
            let response = response.clone();
            move || first.append_resolution(response)
        });
        let right = std::thread::spawn(move || second.append_resolution(response));
        let outcomes = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(ChatError::AlreadyResolved(_))))
                .count(),
            1
        );
        assert_eq!(store.events().unwrap().len(), 2);
    }

    #[test]
    fn a_resolution_cannot_bypass_the_request_or_change_its_responder() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        let answer = question_and_answer(&store);
        assert!(matches!(
            store.append(answer.clone()),
            Err(ChatError::Invalid(_))
        ));
        let mut wrong_responder = answer.clone();
        if let EventData::Message { from, .. } = &mut wrong_responder {
            *from = "carol@a".to_owned();
        }
        assert!(matches!(
            store.append_resolution(wrong_responder),
            Err(ChatError::Invalid(_))
        ));
        let mut wrong_conversation = answer.clone();
        if let EventData::Message { to, .. } = &mut wrong_conversation {
            *to = direct("alice@a", "carol@a");
        }
        assert!(matches!(
            store.append_resolution(wrong_conversation),
            Err(ChatError::Invalid(_))
        ));
        store.append_resolution(answer).unwrap();
    }

    #[test]
    fn three_offline_machines_converge_after_concurrent_group_messages_and_removal() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(&root.path().join("a"), "a".to_owned());
        let b = Store::at(&root.path().join("b"), "b".to_owned());
        let c_state = root.path().join("c");
        let c = Store::at(&c_state, "c".to_owned());
        a.append(EventData::Room {
            id: "room:a:review".to_owned(),
            name: "review".to_owned(),
            members: vec![
                "owner@a".to_owned(),
                "bob@b".to_owned(),
                "carol@c".to_owned(),
            ],
        })
        .unwrap();
        synchronize(&a, &b);
        synchronize(&a, &c);
        let post = |from: &str, text: &str| EventData::Message {
            to: "room:a:review".to_owned(),
            from: from.to_owned(),
            text: text.to_owned(),
            audience: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
                .try_into()
                .unwrap(),
            mode: MessageMode::Send,
        };
        b.append(post("bob@b", "from b while offline")).unwrap();
        c.append(post("carol@c", "from c while offline")).unwrap();
        a.append(EventData::Membership {
            room: "room:a:review".to_owned(),
            member: "carol@c".to_owned(),
            present: false,
        })
        .unwrap();
        synchronize(&a, &b);
        synchronize(&a, &c);
        synchronize(&b, &c);
        drop(c);
        let restarted = Store::at(&c_state, "c".to_owned());
        synchronize(&a, &restarted);
        let ids = |store: &Store| {
            store
                .events()
                .unwrap()
                .into_iter()
                .map(|event| event.id())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&a), ids(&b));
        assert_eq!(ids(&a), ids(&restarted));
        assert!(
            a.rooms()
                .unwrap()
                .get("room:a:review")
                .is_some_and(|room| !room.members.contains("carol@c"))
        );
        assert_eq!(
            a.events()
                .unwrap()
                .into_iter()
                .filter(|event| matches!(event.data, EventData::Message { .. }))
                .count(),
            2
        );
    }
    fn remote_question(store: &Store) -> Event {
        store
            .append(EventData::Message {
                to: direct("alice@a", "bob@b"),
                from: "alice@a".to_owned(),
                text: "question".to_owned(),
                audience: vec!["a".to_owned(), "b".to_owned()].try_into().unwrap(),
                mode: MessageMode::Ask {
                    responder: "bob@b".to_owned(),
                },
            })
            .unwrap()
    }

    fn remote_answer(request: &Event) -> Event {
        let EventData::Message { to, audience, .. } = &request.data else {
            panic!("test request is a message");
        };
        Event {
            origin: "b".to_owned(),
            seq: 1,
            clock: 2,
            data: EventData::Message {
                to: to.clone(),
                from: "bob@b".to_owned(),
                text: "answer".to_owned(),
                audience: audience.clone(),
                mode: MessageMode::Reply {
                    request: request.id(),
                },
            },
        }
    }

    #[test]
    fn sync_validates_parent_responder_and_one_resolution_atomically() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(&root.path().join("a"), "a".to_owned());
        let request = remote_question(&a);
        let answer = remote_answer(&request);
        let mut wrong = answer.clone();
        if let EventData::Message { from, .. } = &mut wrong.data {
            *from = "carol@b".to_owned();
        }
        assert!(matches!(a.merge("b", &[wrong]), Err(ChatError::Invalid(_))));
        assert_eq!(a.seen("b").unwrap(), 0);
        assert_eq!(a.merge("b", std::slice::from_ref(&answer)).unwrap(), 1);
        assert_eq!(a.merge("b", std::slice::from_ref(&answer)).unwrap(), 1);
        let duplicate = Event {
            seq: 2,
            clock: 3,
            ..answer
        };
        assert!(matches!(
            a.merge("b", &[duplicate]),
            Err(ChatError::AlreadyResolved(_))
        ));
        assert_eq!(a.seen("b").unwrap(), 1);
        assert_eq!(a.events().unwrap().len(), 2);
    }

    #[test]
    fn out_of_order_response_delivery_retries_without_advancing_sequence() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(&root.path().join("a"), "a".to_owned());
        let c = Store::at(&root.path().join("c"), "c".to_owned());
        let request = remote_question(&a);
        let answer = remote_answer(&request);
        assert!(matches!(
            c.merge("b", std::slice::from_ref(&answer)),
            Err(ChatError::Unknown(_))
        ));
        assert_eq!(c.seen("b").unwrap(), 0);
        c.merge("a", &[request]).unwrap();
        c.merge("b", &[answer]).unwrap();
        assert_eq!(c.events().unwrap().len(), 2);
    }

    #[test]
    fn turn_claims_survive_restart_and_cannot_claim_unrelated_events() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(&root.path().join("a"), "a".to_owned());
        let b_state = root.path().join("b");
        let b = Store::at(&b_state, "b".to_owned());
        let request = remote_question(&a);
        assert!(matches!(
            a.claim_turn(&request.id()),
            Err(ChatError::Invalid(_))
        ));
        b.merge("a", std::slice::from_ref(&request)).unwrap();
        assert!(b.claim_turn(&request.id()).unwrap());
        assert!(!b.claim_turn(&request.id()).unwrap());
        drop(b);
        let restarted = Store::at(&b_state, "b".to_owned());
        assert_eq!(restarted.pending_turns().unwrap(), vec![request.id()]);
        restarted
            .append_resolution(EventData::TurnFailure {
                request: request.id(),
                to: direct("alice@a", "bob@b"),
                agent: "bob@b".to_owned(),
                audience: vec!["a".to_owned(), "b".to_owned()].try_into().unwrap(),
                failure: TurnFailure::Interrupted,
            })
            .unwrap();
        restarted.finish_turn(&request.id()).unwrap();
        assert!(restarted.pending_turns().unwrap().is_empty());
        assert!(!restarted.claim_turn(&request.id()).unwrap());
    }

    #[test]
    fn the_database_file_passes_the_private_state_boundary() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path(), "a".to_owned());
        store.append(message("alice@a", "private")).unwrap();
        assert!(
            state_io::open_read(&store.root.join("chat.redb"))
                .unwrap()
                .is_some()
        );
        let launch = store.agent_launch_lock("alice@a").unwrap();
        let first = store.try_lock_agent("alice@a").unwrap().unwrap();
        assert!(store.try_lock_agent("alice@a").unwrap().is_none());
        drop(first);
        drop(store.try_lock_agent("alice@a").unwrap().unwrap());
        drop(launch);
    }
    #[test]
    fn notifications_are_durable_and_do_not_claim_managed_turns() {
        let root = tempfile::tempdir().unwrap();
        let a = Store::at(&root.path().join("a"), "a".to_owned());
        let b_state = root.path().join("b");
        let b = Store::at(&b_state, "b".to_owned());
        let request = remote_question(&a);
        b.merge("a", std::slice::from_ref(&request)).unwrap();
        assert!(b.mark_notification(&request.id()).unwrap());
        drop(b);
        let restarted = Store::at(&b_state, "b".to_owned());
        assert!(!restarted.mark_notification(&request.id()).unwrap());
        assert!(restarted.claim_turn(&request.id()).unwrap());
    }
}
