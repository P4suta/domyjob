use std::collections::BTreeMap;

use domyjob_core::chat::event::{Digest, Event, Stamp};
use domyjob_core::chat::exchange::Outbox as _;
use domyjob_core::chat::id::{AgentId, AgentName, Conversation, EventId, Origin};
use redb::{ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::tables::{
    COUNTERS, EVENTS, JOINED, LINKS, LOCAL_AGENTS, OPEN, ORDER, PEERS, READS, THREADS, TOMBSTONES,
    encode, event_key, get_number, get_text, joined_key, order_key, thread_bounds,
};
use super::views::{self, LocalAgent, Stored};
use super::{Store, StoreError, Tx};
use crate::lock::OsLock;
use crate::state_io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LinkState {
    Synced,
    Deferred,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Link {
    pub(crate) state: LinkState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    pub(crate) at: Stamp,
}

fn text_entries(
    table: &impl ReadableTable<&'static str, &'static str>,
) -> Result<Vec<(String, String)>, StoreError> {
    let mut found = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        found.push((key.value().to_owned(), value.value().to_owned()));
    }
    Ok(found)
}

impl Store {
    pub(crate) fn peers(&self) -> Result<BTreeMap<String, Origin>, StoreError> {
        self.read(|read| {
            text_entries(&read.open_table(PEERS)?)?
                .into_iter()
                .map(|(alias, origin)| Ok((alias, Origin::try_from(origin)?)))
                .collect()
        })
    }

    pub(crate) fn pin(
        &self,
        alias: &str,
        origin: &Origin,
        replace: bool,
    ) -> Result<(), StoreError> {
        if origin == self.origin() {
            return Err(StoreError::Corrupt("this machine cannot be its own peer"));
        }
        self.write(|tx| {
            let mut peers = tx.transaction().open_table(PEERS)?;
            match get_text(&peers, alias)? {
                Some(existing) if existing != origin.as_str() && !replace => Err(
                    StoreError::Refused(domyjob_core::chat::policy::Refusal::Invalid(
                        domyjob_core::chat::id::Invalid(
                            "the peer's chat identity changed; confirm with `chat peer replace`",
                        ),
                    )),
                ),
                Some(_) | None => {
                    peers.insert(alias, origin.as_str())?;
                    Ok(())
                }
            }
        })
    }

    pub(crate) fn unpin(&self, alias: &str) -> Result<bool, StoreError> {
        self.write(|tx| {
            let removed = tx.transaction().open_table(PEERS)?.remove(alias)?.is_some();
            tx.transaction().open_table(LINKS)?.remove(alias)?;
            Ok(removed)
        })
    }

    pub(crate) fn links(&self) -> Result<BTreeMap<String, Link>, StoreError> {
        self.read(|read| {
            text_entries(&read.open_table(LINKS)?)?
                .into_iter()
                .map(|(alias, link)| Ok((alias, super::tables::decode(&link)?)))
                .collect()
        })
    }

    pub(crate) fn record_link(&self, alias: &str, link: &Link) -> Result<(), StoreError> {
        self.write(|tx| {
            tx.transaction()
                .open_table(LINKS)?
                .insert(alias, encode(link)?.as_str())?;
            Ok(())
        })
    }

    pub(crate) fn publish_machine(&self) -> Result<(), StoreError> {
        let card = crate::platform::machine_card()?;
        self.write(|tx| {
            let known = views::directory(tx.transaction())?
                .machines
                .get(self.origin())
                .cloned();
            if known.as_ref() != Some(&card) {
                tx.author(
                    domyjob_core::chat::policy::Priority::Ordinary,
                    domyjob_core::chat::event::Body::Machine { card: card.clone() },
                )?;
            }
            Ok(())
        })
    }

    pub(crate) fn clean(&self, conversation: &Conversation) -> Result<usize, StoreError> {
        let lock = OsLock::exclusive(&self.paths().lock())?;
        let mut database = super::open_database(self.paths())?;
        let write = database.begin_write()?;
        let removed = {
            let tx = Tx::new(&write, self.origin());
            clean_in(&tx, conversation)?
        };
        write.commit()?;
        database.compact()?;
        drop(database);
        drop(lock);
        Ok(removed)
    }

    pub(crate) fn reset(state: &crate::layout::State) -> Result<Self, StoreError> {
        let paths = state.chat();
        state_io::private_dir(paths.root())?;
        let lock = OsLock::exclusive(&paths.lock())?;
        state_io::remove_file(&paths.database())?;
        super::super::pulse::forget(&paths)?;
        drop(lock);
        Self::open_in(state)
    }
}

fn thread_events(
    write: &WriteTransaction,
    conversation: &Conversation,
) -> Result<Vec<(String, String)>, StoreError> {
    let (first, last) = thread_bounds(conversation);
    let mut found = Vec::new();
    for entry in write
        .open_table(THREADS)?
        .range(first.as_str()..last.as_str())?
    {
        let (key, id) = entry?;
        found.push((key.value().to_owned(), id.value().to_owned()));
    }
    Ok(found)
}

fn clean_in(tx: &Tx<'_>, conversation: &Conversation) -> Result<usize, StoreError> {
    let write = tx.transaction();
    let entries = thread_events(write, conversation)?;
    let mut events = Vec::new();
    for (_, id) in &entries {
        let id = EventId::try_from(id.clone())?;
        if let Some(Stored::Event { event, encoded }) = views::stored(write, &id)? {
            events.push((*event, encoded));
        }
    }
    for (event, _) in &events {
        let open = write
            .open_table(OPEN)?
            .get(event_key(event.id()).as_str())?
            .is_some();
        if open || !acknowledged(tx, event)? {
            return Err(StoreError::Refused(
                domyjob_core::chat::policy::Refusal::Invalid(domyjob_core::chat::id::Invalid(
                    "the conversation has an open ask or content its peers have not stored yet",
                )),
            ));
        }
    }
    for (key, _) in &entries {
        write.open_table(THREADS)?.remove(key.as_str())?;
    }
    let mut bytes = 0_u64;
    for (event, encoded) in &events {
        let id = event_key(event.id());
        write.open_table(EVENTS)?.remove(id.as_str())?;
        write.open_table(ORDER)?.remove(order_key(event).as_str())?;
        let digest = Digest::of(encoded.as_bytes());
        write
            .open_table(TOMBSTONES)?
            .insert(id.as_str(), (event.clock(), digest.as_bytes().as_slice()))?;
        bytes = bytes.saturating_add(
            u64::try_from(encoded.len()).map_err(|_size| StoreError::Corrupt("event size"))?,
        );
    }
    if let Conversation::Direct(pair) = conversation {
        for agent in pair.agents() {
            write
                .open_table(JOINED)?
                .remove(joined_key(agent, conversation).as_str())?;
        }
    }
    let mut counters = write.open_table(COUNTERS)?;
    let count = u64::try_from(events.len()).map_err(|_size| StoreError::Corrupt("event count"))?;
    let remaining_events = get_number(&counters, "events")?.saturating_sub(count);
    let remaining_bytes = get_number(&counters, "bytes")?.saturating_sub(bytes);
    counters.insert("events", remaining_events)?;
    counters.insert("bytes", remaining_bytes)?;
    Ok(events.len())
}

fn acknowledged(tx: &Tx<'_>, event: &Event) -> Result<bool, StoreError> {
    if event.origin() != tx.local_origin() {
        return Ok(true);
    }
    let Some(audience) = event.body().audience() else {
        return Ok(true);
    };
    for peer in audience
        .members()
        .iter()
        .filter(|peer| *peer != tx.local_origin())
    {
        if tx.ack(peer)? < event.id().seq().get() {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Tx<'_> {
    pub(crate) fn local_origin(&self) -> &Origin {
        domyjob_core::chat::ledger::Ledger::local(self)
    }

    pub(crate) fn configure(
        &self,
        name: &AgentName,
        config: Option<&LocalAgent>,
    ) -> Result<(), StoreError> {
        let mut table = self.transaction().open_table(LOCAL_AGENTS)?;
        match config {
            Some(config) => {
                table.insert(name.as_str(), encode(config)?.as_str())?;
            }
            None => {
                table.remove(name.as_str())?;
            }
        }
        Ok(())
    }

    pub(crate) fn read_cursor(&self, agent: &AgentId) -> Result<String, StoreError> {
        Ok(
            get_text(&self.transaction().open_table(READS)?, &agent.to_string())?
                .unwrap_or_default(),
        )
    }

    pub(crate) fn mark_read(&self, agent: &AgentId, through: &Event) -> Result<(), StoreError> {
        let order = order_key(through);
        if order > self.read_cursor(agent)? {
            self.transaction()
                .open_table(READS)?
                .insert(agent.to_string().as_str(), order.as_str())?;
        }
        Ok(())
    }
}
