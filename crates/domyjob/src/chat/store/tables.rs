use domyjob_core::chat::event::Event;
use domyjob_core::chat::id::{AgentId, Conversation, EventId, RoomId};
use redb::{ReadTransaction, ReadableTable, TableDefinition, WriteTransaction};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::StoreError;

pub(super) type TextTable = TableDefinition<'static, &'static str, &'static str>;

macro_rules! tables {
    ($($(#[$doc:meta])* $name:ident: $key:ty => $value:ty = $label:literal;)+) => {
        $(
            $(#[$doc])*
            pub(super) const $name: TableDefinition<'static, $key, $value> =
                TableDefinition::new($label);
        )+

        pub(super) fn create_all(write: &WriteTransaction) -> Result<(), StoreError> {
            $(drop(write.open_table($name)?);)+
            Ok(())
        }

        #[cfg(test)]
        pub(super) fn manifest() -> Vec<String> {
            vec![$(format!("{}: {} => {}", $label, stringify!($key), stringify!($value))),+]
        }
    };
}

tables! {
    EVENTS: &'static str => &'static str = "events";
    ORDER: &'static str => &'static str = "order";
    THREADS: &'static str => &'static str = "threads";
    JOINED: &'static str => &'static str = "joined";
    CURSORS: &'static str => (u64, u64) = "cursors";
    ACKS: &'static str => u64 = "acks";
    META: &'static str => &'static str = "meta";
    COUNTERS: &'static str => u64 = "counters";
    RESOLUTIONS: &'static str => &'static str = "resolutions";
    OPEN: &'static str => &'static str = "open_asks";
    STARTED: &'static str => &'static str = "started";
    PROFILES: &'static str => &'static str = "profiles";
    MACHINES: &'static str => &'static str = "machines";
    ROOMS: &'static str => &'static str = "rooms";
    TOMBSTONES: &'static str => (u64, &'static [u8]) = "tombstones";
    LOCAL_AGENTS: &'static str => &'static str = "local_agents";
    TURNS: &'static str => &'static str = "turns";
    READS: &'static str => &'static str = "reads";
    PEERS: &'static str => &'static str = "peers";
    LINKS: &'static str => &'static str = "links";
}

const MAX_RECORD: usize = 1024 * 1024;

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

pub(super) fn order_key(event: &Event) -> String {
    format!(
        "{:016x}:{}:{:016x}",
        event.clock(),
        event.origin(),
        event.id().seq()
    )
}

pub(super) fn thread_key(conversation: &Conversation, order: &str) -> String {
    format!("{conversation}\u{1f}{order}")
}

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
