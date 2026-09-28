//! The background service: keeps every pinned peer synchronized while no command runs.
//!
//! Each peer has one worker.
//! It exchanges when this machine stores events or when the peer's long-poll wait reports new events there, and backs off while the peer is unreachable.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use domyjob_core::chat::id::Origin;
use domyjob_core::chat_wire::{ChatReply, ChatRequest};

use super::ops::{self, OpsError};
use super::pulse::{Pulse, PulseError};
use super::store::{Store, StoreError};
use super::sync::{self, Channel, HEARTBEAT_SECONDS, Ssh, SyncError};
use crate::lock::{LockError, OsLock};
use crate::platform::clock::{self, Deadline, Waited};

const EXCHANGE_SECONDS: u64 = 60;
const MAX_BACKOFF_MILLIS: u64 = 60_000;
const PEER_REFRESH_MILLIS: u64 = 60_000;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Pulse(#[from] PulseError),
    #[error(transparent)]
    Ops(#[from] OpsError),
}

enum Signal {
    Local,
    Remote(Result<ChatReply, SyncError>),
    Stop,
}

struct Worker {
    sender: Sender<Signal>,
    thread: JoinHandle<()>,
}

/// Run until the process is stopped; a second service on the same machine exits at once.
pub(crate) fn serve() -> Result<(), ServeError> {
    let store = Store::open()?;
    let Some(_running) = OsLock::try_exclusive(&store.root().join("serve.lock"))? else {
        eprintln!("domyjob: the chat service is already running");
        return Ok(());
    };
    crate::state_io::write_bytes(
        &store.root().join("serve.pid"),
        std::process::id().to_string().as_bytes(),
    )
    .map_err(StoreError::from)?;
    ops::publish_machine(&store)?;
    let mut pulse = Pulse::new(&store, PEER_REFRESH_MILLIS)?;
    let mut workers: BTreeMap<String, Worker> = BTreeMap::new();
    loop {
        reconcile(&store, &mut workers)?;
        if pulse.next(Deadline::after_millis(PEER_REFRESH_MILLIS))? {
            for worker in workers.values() {
                let _delivered = worker.sender.send(Signal::Local);
            }
        }
    }
}

/// Start workers for new peers and stop those of removed or replaced peers.
fn reconcile(store: &Store, workers: &mut BTreeMap<String, Worker>) -> Result<(), ServeError> {
    let peers = store.peers()?;
    let stale: Vec<String> = workers
        .keys()
        .filter(|alias| !peers.contains_key(*alias))
        .cloned()
        .collect();
    for alias in stale {
        if let Some(worker) = workers.remove(&alias) {
            let _delivered = worker.sender.send(Signal::Stop);
            let _joined = worker.thread.join();
        }
    }
    for (alias, origin) in peers {
        if workers.contains_key(&alias) {
            continue;
        }
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn({
            let store = store.clone();
            let sender = sender.clone();
            let alias = alias.clone();
            move || {
                let peer = Peer {
                    alias: &alias,
                    origin: &origin,
                };
                follow(&store, &peer, &sender, &receiver);
            }
        });
        workers.insert(alias, Worker { sender, thread });
    }
    Ok(())
}

/// Hold one long-poll wait on the peer and report its result to the worker.
fn start_wait(store: &Store, alias: &str, peer: &Origin, sender: &Sender<Signal>) {
    let request = store.read(|read| Ok(super::store::cursor(read, peer)?.seen));
    let (store_origin, alias, peer, sender) = (
        store.origin().clone(),
        alias.to_owned(),
        peer.clone(),
        sender.clone(),
    );
    std::thread::spawn(move || {
        let result = request.map_err(SyncError::from).and_then(|seen| {
            let mut channel = Ssh::new(&alias)?;
            let deadline = Deadline::after_seconds(HEARTBEAT_SECONDS.saturating_add(15));
            channel.call(
                ChatRequest::Wait {
                    from: store_origin,
                    to: peer,
                    seen,
                },
                deadline,
            )
        });
        let _delivered = sender.send(Signal::Remote(result));
    });
}

/// One pinned peer, by SSH alias and chat identity.
struct Peer<'a> {
    alias: &'a str,
    origin: &'a Origin,
}

/// One peer's loop: exchange, then wait for either side to change, backing off on failure.
fn follow(store: &Store, peer: &Peer<'_>, sender: &Sender<Signal>, receiver: &Receiver<Signal>) {
    let Peer {
        alias,
        origin: peer,
    } = *peer;
    let mut backoff = 1000_u64;
    let mut waiting = false;
    let mut exchange = true;
    loop {
        if exchange {
            let deadline = Deadline::after_seconds(EXCHANGE_SECONDS);
            match sync::sync(store, Some(alias), deadline) {
                Ok(report) if report.complete() => backoff = 1000,
                Ok(_) | Err(_) => {
                    clock::pause_millis(backoff);
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF_MILLIS);
                }
            }
        }
        if !waiting {
            start_wait(store, alias, peer, sender);
            waiting = true;
        }
        exchange = match clock::receive(receiver, Deadline::after_millis(MAX_BACKOFF_MILLIS)) {
            Waited::Received(Signal::Stop) | Waited::Closed => return,
            Waited::Received(Signal::Local) | Waited::Expired => true,
            Waited::Received(Signal::Remote(result)) => {
                waiting = false;
                match result {
                    Ok(ChatReply::Heartbeat {}) => false,
                    Ok(_) => true,
                    Err(_) => {
                        clock::pause_millis(backoff);
                        backoff = backoff.saturating_mul(2).min(MAX_BACKOFF_MILLIS);
                        true
                    }
                }
            }
        };
    }
}
