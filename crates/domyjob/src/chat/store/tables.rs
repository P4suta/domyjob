//! Table definitions, key encodings, and the codec for stored chat records.

use domyjob_core::chat::event::Event;
use domyjob_core::chat::id::{AgentId, Conversation, EventId, RoomId};
use redb::{ReadTransaction, ReadableTable, TableDefinition, WriteTransaction};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::StoreError;

pub(super) type TextTable = TableDefinition<'static, &'static str, &'static str>;
type Number = TableDefinition<'static, &'static str, u64>;

/// Event ID → canonical encoding.
pub(super) const EVENTS: TextTable = TableDefinition::new("events");
/// Display order key → event ID.
pub(super) const ORDER: TextTable = TableDefinition::new("order");
/// Conversation and order key → event ID.
pub(super) const THREADS: TextTable = TableDefinition::new("threads");
/// Agent and direct conversation → empty, for inbox lookup.
pub(super) const JOINED: TextTable = TableDefinition::new("joined");
/// Origin → (stored sequence, last clock).
pub(super) const CURSORS: TableDefinition<'static, &'static str, (u64, u64)> =
    TableDefinition::new("cursors");
/// Peer origin → highest own sequence the peer stored.
pub(super) const ACKS: Number = TableDefinition::new("acks");
/// `schema` and `origin`.
pub(super) const META: TextTable = TableDefinition::new("meta");
/// `clock`, `events`, `bytes`, and `generation`.
pub(super) const COUNTERS: Number = TableDefinition::new("counters");
/// Ask ID → winning resolution.
pub(super) const RESOLUTIONS: TextTable = TableDefinition::new("resolutions");
/// Unresolved ask ID → responder.
pub(super) const OPEN: TextTable = TableDefinition::new("open_asks");
/// Ask ID → the responder that started it.
pub(super) const STARTED: TextTable = TableDefinition::new("started");
/// Agent → profile card.
pub(super) const PROFILES: TextTable = TableDefinition::new("profiles");
/// Origin → machine card.
pub(super) const MACHINES: TextTable = TableDefinition::new("machines");
/// Room conversation → room state.
pub(super) const ROOMS: TextTable = TableDefinition::new("rooms");
/// Cleaned event ID → (clock, content digest).
pub(super) const TOMBSTONES: TableDefinition<'static, &'static str, (u64, &'static [u8])> =
    TableDefinition::new("tombstones");
/// Local agent name → private configuration.
pub(super) const LOCAL_AGENTS: TextTable = TableDefinition::new("local_agents");
/// Claimed ask ID → local agent.
pub(super) const TURNS: TextTable = TableDefinition::new("turns");
/// Local agent → order key of the last message it read.
pub(super) const READS: TextTable = TableDefinition::new("reads");
/// SSH alias → pinned peer origin.
pub(super) const PEERS: TextTable = TableDefinition::new("peers");
/// SSH alias → last synchronization outcome.
pub(super) const LINKS: TextTable = TableDefinition::new("links");

pub(super) const SCHEMA: &str = "chat-v2";
/// The largest stored record the codec decodes.
const MAX_RECORD: usize = 1024 * 1024;

/// Create every table so later read transactions never meet a missing one.
pub(super) fn create_all(write: &WriteTransaction) -> Result<(), StoreError> {
    for text in [
        EVENTS,
        ORDER,
        THREADS,
        JOINED,
        META,
        RESOLUTIONS,
        OPEN,
        STARTED,
        PROFILES,
        MACHINES,
        ROOMS,
        LOCAL_AGENTS,
        TURNS,
        READS,
        PEERS,
        LINKS,
    ] {
        drop(write.open_table(text)?);
    }
    for number in [ACKS, COUNTERS] {
        drop(write.open_table(number)?);
    }
    drop(write.open_table(CURSORS)?);
    drop(write.open_table(TOMBSTONES)?);
    Ok(())
}

/// Read access shared by read and write transactions.
pub(crate) trait Reader {
    fn table<K: redb::Key + 'static, V: redb::Value + 'static>(
        &self,
        definition: TableDefinition<'static, K, V>,
    ) -> Result<impl ReadableTable<K, V>, StoreError>;
}

impl Reader for ReadTransaction {
    fn table<K: redb::Key + 'static, V: redb::Value + 'static>(
        &self,
        definition: TableDefinition<'static, K, V>,
    ) -> Result<impl ReadableTable<K, V>, StoreError> {
        Ok(self.open_table(definition)?)
    }
}

impl Reader for WriteTransaction {
    fn table<K: redb::Key + 'static, V: redb::Value + 'static>(
        &self,
        definition: TableDefinition<'static, K, V>,
    ) -> Result<impl ReadableTable<K, V>, StoreError> {
        Ok(self.open_table(definition)?)
    }
}

pub(super) fn decode<T: DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    Ok(domyjob_core::ingress::json(text.as_bytes(), MAX_RECORD)?)
}

pub(super) fn encode<T: Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

/// Decode the text value stored under `key`.
pub(super) fn get_json<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static str>,
    key: &str,
) -> Result<Option<T>, StoreError> {
    table
        .get(key)?
        .map(|value| decode(value.value()))
        .transpose()
}

pub(super) fn get_text(
    table: &impl ReadableTable<&'static str, &'static str>,
    key: &str,
) -> Result<Option<String>, StoreError> {
    Ok(table.get(key)?.map(|value| value.value().to_owned()))
}

pub(super) fn get_number(
    table: &impl ReadableTable<&'static str, u64>,
    key: &str,
) -> Result<u64, StoreError> {
    Ok(table.get(key)?.map_or(0, |value| value.value()))
}

/// The shared display order: clock, origin, then sequence, in fixed-width text.
pub(super) fn order_key(event: &Event) -> String {
    format!(
        "{:016x}:{}:{:016x}",
        event.clock(),
        event.origin(),
        event.id().seq()
    )
}

/// A thread entry: the conversation, a unit separator, then the order key.
pub(super) fn thread_key(conversation: &Conversation, order: &str) -> String {
    format!("{conversation}\u{1f}{order}")
}

/// The first and last thread keys of a conversation.
pub(super) fn thread_bounds(conversation: &Conversation) -> (String, String) {
    (
        format!("{conversation}\u{1f}"),
        format!("{conversation}\u{1f}\u{7f}"),
    )
}

pub(super) fn joined_key(agent: &AgentId, conversation: &Conversation) -> String {
    format!("{agent}\u{1f}{conversation}")
}

pub(super) fn room_key(room: &RoomId) -> String {
    Conversation::Room(room.clone()).to_string()
}

pub(super) fn event_key(id: &EventId) -> String {
    id.to_string()
}
