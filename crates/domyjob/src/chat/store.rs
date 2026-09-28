//! The durable chat ledger: one redb file per machine, opened only under its OS lock.

use domyjob_core::chat::event::{Body, Event};
use domyjob_core::chat::exchange::{self, Progress};
use domyjob_core::chat::id::{AgentId, Invalid, Origin};
use domyjob_core::chat::ledger::{self, Failure, Rejection};
use domyjob_core::chat::policy::{self, Priority, Refusal};
use domyjob_core::chat_wire::{Answer, Offer};
use redb::{Database, ReadTransaction, ReadableDatabase, WriteTransaction};

use crate::layout::{self, AgentLock};
use crate::lock::{LockError, OsLock};
use crate::state_io::{self, StateError};

mod admin;
mod tables;
mod turns;
mod tx;
mod views;

pub(crate) use admin::{Link, LinkState};
pub(crate) use tables::Reader;
pub(crate) use turns::{Completion, Turn};
pub(crate) use tx::Tx;
pub(crate) use views::{
    Directory, LocalAgent, Presence, agent_config, cursor, directory, event, inbox, local_agents,
    open_asks, profile, resolution, room, rooms, thread,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("chat state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("chat identity entropy failed: {0}")]
    Entropy(getrandom::Error),
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
    Compaction(#[from] redb::CompactionError),
    #[error("chat record encoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("a stored chat record is corrupt: {0}")]
    Record(#[from] domyjob_core::ingress::JsonError),
    #[error("stored chat data is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("the chat store was reset or replaced; restart this process")]
    IdentityChanged,
    #[error(
        "the chat store was written in format {0}, which this build cannot read; \
         run a build of that format, or `domyjob chat reset --yes` to start a new history"
    )]
    Format(String),
    #[error(transparent)]
    Rejected(#[from] Rejection),
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error("no chat agent, room, or message matches {0}")]
    Unknown(String),
    #[error("{0} matches more than one agent or room; qualify it with @MACHINE")]
    Ambiguous(String),
}

impl From<Failure<Self>> for StoreError {
    fn from(failure: Failure<Self>) -> Self {
        match failure {
            Failure::Store(error) => error,
            Failure::Rejected(rejection) => Self::Rejected(rejection),
            Failure::Invalid(invalid) => Self::Invalid(invalid),
        }
    }
}

/// A handle on this machine's chat ledger; every operation opens the file under the lock.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    paths: layout::Chat,
    origin: Origin,
}

fn open_database(paths: &layout::Chat) -> Result<Database, StoreError> {
    let file = state_io::open_lock(&paths.database())?;
    Ok(Database::builder().create_file(file)?)
}

fn new_origin() -> Result<Origin, StoreError> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(StoreError::Entropy)?;
    Ok(Origin::from_entropy(entropy))
}

/// Read or create the store's identity, refusing a store written in another format.
fn initialize(database: &Database) -> Result<Origin, StoreError> {
    let write = database.begin_write()?;
    tables::create_all(&write)?;
    let origin = {
        let mut meta = write.open_table(tables::META)?;
        let format = crate::formats::chat();
        let known = tables::get_text(&meta, "origin")?.is_some();
        match tables::get_text(&meta, "format")? {
            Some(found) if found != format => return Err(StoreError::Format(found)),
            Some(_) => {}
            // A store with an identity but no format was written before formats were recorded.
            None if known => return Err(StoreError::Format("unrecorded".to_owned())),
            None => {
                meta.insert("format", format.as_str())?;
            }
        }
        if let Some(text) = tables::get_text(&meta, "origin")? {
            Origin::try_from(text)?
        } else {
            let origin = new_origin()?;
            meta.insert("origin", origin.as_str())?;
            origin
        }
    };
    write.commit()?;
    Ok(origin)
}

impl Store {
    pub(crate) fn open() -> Result<Self, StoreError> {
        Self::open_in(&layout::State::here()?)
    }

    /// Open or create the chat store inside a private state directory.
    pub(crate) fn open_in(state: &layout::State) -> Result<Self, StoreError> {
        let paths = state.chat();
        state_io::private_dir(paths.root())?;
        let lock = OsLock::exclusive(&paths.lock())?;
        let origin = initialize(&open_database(&paths)?)?;
        drop(lock);
        Ok(Self { paths, origin })
    }

    #[must_use]
    pub(crate) const fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Remove the recorded format, as a store written before formats were recorded has none.
    #[cfg(test)]
    pub(crate) fn forget_format_for_test(&self) -> Result<(), StoreError> {
        let database = open_database(&self.paths)?;
        let write = database.begin_write()?;
        write.open_table(tables::META)?.remove("format")?;
        write.commit()?;
        Ok(())
    }

    /// Where this store's files live.
    #[must_use]
    pub(crate) const fn paths(&self) -> &layout::Chat {
        &self.paths
    }

    /// Serialize worker launches and final queue scans of one agent.
    pub(crate) fn launch_lock(&self, agent: &AgentId) -> Result<OsLock, StoreError> {
        Ok(OsLock::exclusive(
            &self.paths.agent_lock(agent, AgentLock::Launch),
        )?)
    }

    /// The lock a running worker holds for its agent's whole queue.
    pub(crate) fn try_agent_lock(&self, agent: &AgentId) -> Result<Option<OsLock>, StoreError> {
        Ok(OsLock::try_exclusive(
            &self.paths.agent_lock(agent, AgentLock::Queue),
        )?)
    }

    fn verify(&self, reader: &impl Reader) -> Result<(), StoreError> {
        let meta = reader.table(tables::META)?;
        if tables::get_text(&meta, "origin")?.as_deref() != Some(self.origin.as_str()) {
            return Err(StoreError::IdentityChanged);
        }
        Ok(())
    }

    pub(crate) fn read<T>(
        &self,
        work: impl FnOnce(&ReadTransaction) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let lock = OsLock::exclusive(&self.paths.lock())?;
        let database = open_database(&self.paths)?;
        let read = database.begin_read()?;
        self.verify(&read)?;
        let value = work(&read);
        drop(read);
        drop(database);
        drop(lock);
        value
    }

    /// Run `work` in one write transaction; ring the doorbell when it stored events.
    pub(crate) fn write<T>(
        &self,
        work: impl FnOnce(&mut Tx<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let lock = OsLock::exclusive(&self.paths.lock())?;
        let database = open_database(&self.paths)?;
        let write: WriteTransaction = database.begin_write()?;
        self.verify(&write)?;
        let mut tx = Tx::new(&write, &self.origin);
        let value = work(&mut tx)?;
        let generation = tx.generation()?;
        let appended = tx.appended();
        write.commit()?;
        drop(database);
        if appended {
            super::pulse::ring(&self.paths, generation)?;
        }
        drop(lock);
        Ok(value)
    }

    pub(crate) fn offer(&self, peer: &Origin) -> Result<Result<Offer, Rejection>, StoreError> {
        self.write(|tx| exchange::offer(tx, peer))
    }

    pub(crate) fn respond(&self, offer: &Offer) -> Result<Result<Answer, Rejection>, StoreError> {
        self.write(|tx| exchange::respond(tx, offer))
    }

    pub(crate) fn accept(
        &self,
        offer: &Offer,
        answer: Answer,
    ) -> Result<Result<Progress, Rejection>, StoreError> {
        self.write(|tx| exchange::accept(tx, offer, answer))
    }

    /// The last sequence this machine authored.
    pub(crate) fn local_seen(&self) -> Result<u64, StoreError> {
        self.read(|read| Ok(cursor(read, &self.origin)?.seen))
    }
}

impl Tx<'_> {
    pub(crate) fn author(&mut self, priority: Priority, body: Body) -> Result<Event, StoreError> {
        policy::check_capacity(views::usage(self.transaction())?, priority)?;
        Ok(ledger::append(self, body)?)
    }
}

#[cfg(test)]
mod specimen;
#[cfg(test)]
mod tests;
