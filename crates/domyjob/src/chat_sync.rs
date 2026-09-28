#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use domyjob_core::chat_wire::{BATCH, Batch, ChatReply, ChatRequest, Origin};
use domyjob_core::domain::MachineName;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chat::{ChatError, Store};
use crate::chat_runner::{self, RunnerError};
use crate::lock::{LockError, OsLock};
use crate::platform;
use crate::state_io::{self, StateError};
use crate::transport::{self, TransportError};

pub(crate) use crate::platform::chat_poll::{WaitWindow, wait_tick};

const MAX_PEERS: usize = 64;
const SYNC_ROUNDS: usize = 32;

#[derive(Debug, Error)]
pub(crate) enum ServerError {
    #[error(transparent)]
    Chat(#[from] ChatError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("invalid chat exchange: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Error)]
pub(crate) enum SyncError {
    #[error(transparent)]
    Chat(#[from] ChatError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Machine(#[from] domyjob_core::domain::Invalid),
    #[error("chat configuration I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid chat configuration: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid chat synchronization: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Peers {
    machines: BTreeMap<String, Origin>,
}

fn directory() -> Result<PathBuf, SyncError> {
    let state = platform::state()?.join("v1").join("chat");
    state_io::private_dir(&state)?;
    Ok(state)
}

#[expect(
    clippy::disallowed_methods,
    reason = "private peer configuration is decoded only after the bounded state read"
)]
fn load() -> Result<Peers, SyncError> {
    let Some(bytes) = state_io::read_bytes(&directory()?.join("peers.json"))? else {
        return Ok(Peers::default());
    };
    let peers: Peers = serde_json::from_slice(&bytes)?;
    if peers.machines.len() > MAX_PEERS {
        return Err(SyncError::Invalid("too many peers"));
    }
    for name in peers.machines.keys() {
        let _validated = MachineName::try_from(name.clone())?;
    }
    Ok(peers)
}

pub(crate) fn peers() -> Result<BTreeMap<String, String>, SyncError> {
    Ok(load()?
        .machines
        .into_iter()
        .map(|(name, origin)| (name, String::from(origin)))
        .collect())
}

fn identity(machine: &MachineName) -> Result<Origin, SyncError> {
    match transport::chat(machine, ChatRequest::Identity {})? {
        ChatReply::Identity { origin } => Ok(origin),
        ChatReply::Exchanged { .. } => Err(SyncError::Invalid("expected peer identity")),
    }
}

pub(crate) fn setup(machines: &[String]) -> Result<(), SyncError> {
    let store = Store::open()?;
    if machines.len() > MAX_PEERS {
        return Err(SyncError::Invalid("too many peers"));
    }
    let mut additions = BTreeMap::new();
    for name in machines {
        let machine = MachineName::try_from(name.clone())?;
        if name == "local" {
            return Err(SyncError::Invalid("local is reserved for this machine"));
        }
        let origin = identity(&machine)?;
        if origin.as_str() == store.origin() {
            return Err(SyncError::Invalid(
                "cannot add this machine as its own peer",
            ));
        }
        additions.insert(name.clone(), origin);
    }
    let directory = directory()?;
    let _lock = OsLock::exclusive(&directory.join("peers.lock"))?;
    let mut peers = load()?;
    for (name, origin) in additions {
        if peers.machines.get(&name).is_some_and(|old| old != &origin) {
            return Err(SyncError::Invalid(
                "peer identity changed; review peers.json before replacing it",
            ));
        }
        peers.machines.insert(name, origin);
    }
    if peers.machines.len() > MAX_PEERS {
        return Err(SyncError::Invalid("too many peers"));
    }
    state_io::write_bytes(&directory.join("peers.json"), &serde_json::to_vec(&peers)?)?;
    Ok(())
}

pub(crate) fn handle(request: ChatRequest) -> Result<ChatReply, ServerError> {
    let store = Store::open()?;
    let local = Origin::try_from(store.origin().to_owned()).map_err(ServerError::Invalid)?;
    match request {
        ChatRequest::Identity {} => Ok(ChatReply::Identity { origin: local }),
        ChatRequest::Exchange {
            origin,
            after,
            seen,
            events,
        } => {
            // SSH authenticates the account, which already has full access to this state.
            // Acknowledgments are checked against local history before any incoming write.
            if seen > store.seen(store.origin())? {
                return Err(ServerError::Invalid("peer cursor exceeds local history"));
            }
            let through = validate_batch(after, events.events()).map_err(ServerError::Invalid)?;
            let received = store.merge(origin.as_str(), events.events())?;
            if received < through {
                return Err(ServerError::Invalid(
                    "sender cursor exceeds received history",
                ));
            }
            store.record_ack(origin.as_str(), seen)?;
            chat_runner::dispatch(&store)?;
            let outgoing = store.after_for(seen, BATCH, origin.as_str())?;
            Ok(ChatReply::Exchanged {
                origin: local,
                seen: through,
                events: Batch::try_from(outgoing).map_err(ServerError::Invalid)?,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncState {
    Synced,
    Partial,
    Failed,
}

#[derive(Debug, Serialize)]
pub(crate) struct PeerReport {
    pub(crate) machine: String,
    pub(crate) state: SyncState,
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SyncReport {
    pub(crate) peers: Vec<PeerReport>,
}

impl SyncReport {
    pub(crate) fn is_complete(&self) -> bool {
        self.peers
            .iter()
            .all(|peer| peer.state == SyncState::Synced)
    }
}

fn exchange(store: &Store, machine: &MachineName, peer: &Origin) -> Result<SyncState, SyncError> {
    if identity(machine)? != *peer {
        return Err(SyncError::Invalid(
            "peer identity changed; no message content was sent",
        ));
    }
    let local = Origin::try_from(store.origin().to_owned()).map_err(SyncError::Invalid)?;
    for _ in 0..SYNC_ROUNDS {
        let after = store.ack(peer.as_str())?;
        let outgoing = store.after_for(after, BATCH, peer.as_str())?;
        let through = validate_batch(after, &outgoing).map_err(SyncError::Invalid)?;
        let sent_count = outgoing.len();
        let request = ChatRequest::Exchange {
            origin: local.clone(),
            after,
            seen: store.seen(peer.as_str())?,
            events: Batch::try_from(outgoing).map_err(SyncError::Invalid)?,
        };
        let ChatReply::Exchanged {
            origin,
            seen,
            events,
        } = transport::chat(machine, request)?
        else {
            return Err(SyncError::Invalid("expected exchange reply"));
        };
        if origin != *peer || seen != through {
            return Err(SyncError::Invalid(
                "peer identity or acknowledgment mismatch",
            ));
        }
        store.merge(peer.as_str(), events.events())?;
        store.record_ack(peer.as_str(), seen)?;
        if sent_count < BATCH && events.events().len() < BATCH {
            return Ok(SyncState::Synced);
        }
    }
    Ok(SyncState::Partial)
}

fn validate_batch(after: u64, events: &[crate::chat::Event]) -> Result<u64, &'static str> {
    let mut through = after;
    for event in events {
        through = through.checked_add(1).ok_or("sender sequence exhausted")?;
        if event.seq != through {
            return Err("outgoing batch is not contiguous with its cursor");
        }
    }
    Ok(through)
}

pub(crate) fn sync(store: &Store, machine: Option<&str>) -> Result<SyncReport, SyncError> {
    let peers = load()?;
    if machine.is_some_and(|name| !peers.machines.contains_key(name)) {
        return Err(SyncError::Invalid(
            "unknown peer; run chat setup MACHINE first",
        ));
    }
    let mut report = SyncReport { peers: Vec::new() };
    for (name, origin) in peers.machines {
        if machine.is_some_and(|selected| selected != name) {
            continue;
        }
        let result = exchange(store, &MachineName::try_from(name.clone())?, &origin);
        let (state, detail) = match result {
            Ok(state) => (state, None),
            Err(error) => (SyncState::Failed, Some(error.to_string())),
        };
        report.peers.push(PeerReport {
            machine: name,
            state,
            detail,
        });
    }
    chat_runner::dispatch(store)?;
    Ok(report)
}
