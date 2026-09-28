//! Table definitions, key encodings, and the codec for stored chat records.

use domyjob_core::chat::event::Event;
use domyjob_core::chat::id::{AgentId, Conversation, EventId, RoomId};
use redb::{ReadTransaction, ReadableTable, TableDefinition, WriteTransaction};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::StoreError;

pub(super) type TextTable = TableDefinition<'static, &'static str, &'static str>;

/// Declares every table once: its constant, its creation, and its line in the format specimen.
macro_rules! tables {
    ($($(#[$doc:meta])* $name:ident: $key:ty => $value:ty = $label:literal;)+) => {
        $(
            $(#[$doc])*
            pub(super) const $name: TableDefinition<'static, $key, $value> =
                TableDefinition::new($label);
        )+

        /// Create every table so later read transactions never meet a missing one.
        pub(super) fn create_all(write: &WriteTransaction) -> Result<(), StoreError> {
            $(drop(write.open_table($name)?);)+
            Ok(())
        }

        /// Every table with its key and value types.
        #[cfg(test)]
        pub(super) fn manifest() -> Vec<String> {
            vec![$(format!("{}: {} => {}", $label, stringify!($key), stringify!($value))),+]
        }
    };
}

tables! {
    /// Event ID → canonical encoding.
    EVENTS: &'static str => &'static str = "events";
    /// Display order key → event ID.
    ORDER: &'static str => &'static str = "order";
    /// Conversation and order key → event ID.
    THREADS: &'static str => &'static str = "threads";
    /// Agent and direct conversation → empty, for inbox lookup.
    JOINED: &'static str => &'static str = "joined";
    /// Origin → (stored sequence, last clock).
    CURSORS: &'static str => (u64, u64) = "cursors";
    /// Peer origin → highest own sequence the peer stored.
    ACKS: &'static str => u64 = "acks";
    /// `format` and `origin`.
    META: &'static str => &'static str = "meta";
    /// `clock`, `events`, `bytes`, and `generation`.
    COUNTERS: &'static str => u64 = "counters";
    /// Ask ID → winning resolution.
    RESOLUTIONS: &'static str => &'static str = "resolutions";
    /// Unresolved ask ID → responder.
    OPEN: &'static str => &'static str = "open_asks";
    /// Ask ID → the responder that started it.
    STARTED: &'static str => &'static str = "started";
    /// Agent → profile card.
    PROFILES: &'static str => &'static str = "profiles";
    /// Origin → machine card.
    MACHINES: &'static str => &'static str = "machines";
    /// Room conversation → room state.
    ROOMS: &'static str => &'static str = "rooms";
    /// Cleaned event ID → (clock, content digest).
    TOMBSTONES: &'static str => (u64, &'static [u8]) = "tombstones";
    /// Local agent name → private configuration.
    LOCAL_AGENTS: &'static str => &'static str = "local_agents";
    /// Claimed ask ID → local agent.
    TURNS: &'static str => &'static str = "turns";
    /// Local agent → order key of the last message it read.
    READS: &'static str => &'static str = "reads";
    /// SSH alias → pinned peer origin.
    PEERS: &'static str => &'static str = "peers";
    /// SSH alias → last synchronization outcome.
    LINKS: &'static str => &'static str = "links";
}

/// The largest stored record the codec decodes.
const MAX_RECORD: usize = 1024 * 1024;

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
