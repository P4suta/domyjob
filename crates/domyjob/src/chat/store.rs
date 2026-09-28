//! The durable chat ledger: one redb file per machine, opened only under its OS lock.

use std::path::{Path, PathBuf};

use domyjob_core::chat::event::{Body, Event};
use domyjob_core::chat::exchange::{self, Progress};
use domyjob_core::chat::id::{AgentId, Invalid, Origin};
use domyjob_core::chat::ledger::{self, Failure, Rejection};
use domyjob_core::chat::policy::{self, Priority, Refusal};
use domyjob_core::chat_wire::{Answer, Offer};
use redb::{Database, ReadTransaction, ReadableDatabase, WriteTransaction};

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
    #[error("stored chat data is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("the chat store was reset or replaced; restart this process")]
    IdentityChanged,
    #[error(
        "the chat store has schema {0}; run the matching domyjob build or `domyjob chat reset`"
    )]
    Schema(String),
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
    root: PathBuf,
    origin: Origin,
}

fn open_database(root: &Path) -> Result<Database, StoreError> {
    let file = state_io::open_lock(&root.join("chat.redb"))?;
    Ok(Database::builder().create_file(file)?)
}

fn new_origin() -> Result<Origin, StoreError> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(StoreError::Entropy)?;
    Ok(Origin::from_entropy(entropy))
}

/// Read or create the store's identity, refusing a store written by another schema.
fn initialize(database: &Database) -> Result<Origin, StoreError> {
    let write = database.begin_write()?;
    tables::create_all(&write)?;
    let origin = {
        let mut meta = write.open_table(tables::META)?;
        match tables::get_text(&meta, "schema")? {
            Some(schema) if schema != tables::SCHEMA => return Err(StoreError::Schema(schema)),
            Some(_) => {}
            None => {
                meta.insert("schema", tables::SCHEMA)?;
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
        Self::open_in(&crate::platform::state()?.join("v1"))
    }

    /// Open or create the chat store inside a private state directory.
    pub(crate) fn open_in(state: &Path) -> Result<Self, StoreError> {
        let root = state.join("chat");
        state_io::private_dir(&root)?;
        let lock = OsLock::exclusive(&root.join("chat.lock"))?;
        let origin = initialize(&open_database(&root)?)?;
        drop(lock);
        Ok(Self { root, origin })
    }

    #[must_use]
    pub(crate) const fn origin(&self) -> &Origin {
        &self.origin
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn agent_lock_path(&self, agent: &AgentId, kind: &str) -> PathBuf {
        let digest = blake3::hash(agent.to_string().as_bytes());
        self.root.join(format!("{kind}-{}.lock", digest.to_hex()))
    }

    /// Serialize worker launches and final queue scans of one agent.
    pub(crate) fn launch_lock(&self, agent: &AgentId) -> Result<OsLock, StoreError> {
        Ok(OsLock::exclusive(&self.agent_lock_path(agent, "launch"))?)
    }

    /// The lock a running worker holds for its agent's whole queue.
    pub(crate) fn try_agent_lock(&self, agent: &AgentId) -> Result<Option<OsLock>, StoreError> {
        Ok(OsLock::try_exclusive(
            &self.agent_lock_path(agent, "agent"),
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
        let lock = OsLock::exclusive(&self.root.join("chat.lock"))?;
        let database = open_database(&self.root)?;
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
        let lock = OsLock::exclusive(&self.root.join("chat.lock"))?;
        let database = open_database(&self.root)?;
        let write: WriteTransaction = database.begin_write()?;
        self.verify(&write)?;
        let mut tx = Tx::new(&write, &self.origin);
        let value = work(&mut tx)?;
        let generation = tx.generation()?;
        let appended = tx.appended();
        write.commit()?;
        drop(database);
        if appended {
            super::bell::ring(&self.root, generation)?;
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
mod tests;
