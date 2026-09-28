//! Exchanging events with pinned peers, and answering their exchanges and waits.

use std::sync::atomic::{AtomicBool, Ordering};

use domyjob_core::chat::id::Origin;
use domyjob_core::chat::ledger::Rejection;
use domyjob_core::chat_wire::{ChatReply, ChatRequest};
use domyjob_core::domain::MachineName;
use serde::Serialize;

use super::pulse::{Pulse, PulseError};
use super::runner::{self, RunnerError};
use super::store::{Link, LinkState, Store, StoreError};
use crate::platform::clock::{self, Deadline};
use crate::transport::{self, TransportError};

/// Rounds of one exchange with one peer before reporting partial progress.
const MAX_ROUNDS: usize = 64;
/// How long a node holds a wait before answering with a heartbeat.
pub(crate) const HEARTBEAT_SECONDS: u64 = 25;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SyncError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Pulse(#[from] PulseError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    Machine(#[from] domyjob_core::domain::Invalid),
    #[error(transparent)]
    Rejected(#[from] Rejection),
    #[error("the peer answered with an unexpected chat reply")]
    Unexpected,
    #[error("{0} is not a chat peer; run `domyjob chat setup {0}`")]
    UnknownPeer(String),
}

/// A way to send one chat request to a peer.
pub(crate) trait Channel {
    fn call(&mut self, request: ChatRequest, deadline: Deadline) -> Result<ChatReply, SyncError>;
}

/// The OpenSSH channel to a machine alias.
#[derive(Debug)]
pub(crate) struct Ssh(MachineName);

impl Ssh {
    pub(crate) fn new(alias: &str) -> Result<Self, SyncError> {
        Ok(Self(MachineName::try_from(alias.to_owned())?))
    }
}

impl Channel for Ssh {
    fn call(&mut self, request: ChatRequest, deadline: Deadline) -> Result<ChatReply, SyncError> {
        Ok(transport::chat(&self.0, &request, deadline)?)
    }
}

/// How one peer's exchange ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Synced,
    Partial,
    /// The peer or this machine waits for an event from a third machine.
    Deferred(Rejection),
    Failed(String),
}

fn classify(rejection: Rejection) -> Outcome {
    match rejection {
        Rejection::MissingDependency { .. } => Outcome::Deferred(rejection),
        Rejection::WrongPeer => Outcome::Failed(
            "the peer's chat identity changed; confirm it with `domyjob chat peer replace`"
                .to_owned(),
        ),
        Rejection::Gap { .. }
        | Rejection::Conflict { .. }
        | Rejection::ClockRegression { .. }
        | Rejection::Mismatch { .. }
        | Rejection::Unauthorized { .. }
        | Rejection::ResourceLimit
        | Rejection::AheadOfHistory
        | Rejection::Inconsistent => Outcome::Failed(rejection.to_string()),
    }
}

/// Exchange rounds with one peer until neither side has more to send.
fn exchange(
    store: &Store,
    peer: &Origin,
    channel: &mut impl Channel,
    deadline: Deadline,
) -> Result<(Outcome, usize), SyncError> {
    let mut appended = 0_usize;
    for _ in 0..MAX_ROUNDS {
        let offer = store.offer(peer)??;
        let answer = match channel.call(ChatRequest::Exchange(offer.clone()), deadline)? {
            ChatReply::Exchanged(answer) => answer,
            ChatReply::Rejected { rejection } => return Ok((classify(rejection), appended)),
            ChatReply::Identity { .. } | ChatReply::Changed {} | ChatReply::Heartbeat {} => {
                return Err(SyncError::Unexpected);
            }
        };
        let progress = store.accept(&offer, answer)??;
        appended = appended.saturating_add(progress.received.appended);
        if let Some(rejection) = progress.refused.or(progress.received.rejected) {
            return Ok((classify(rejection), appended));
        }
        if !progress.more {
            return Ok((Outcome::Synced, appended));
        }
    }
    Ok((Outcome::Partial, appended))
}

/// One peer's line of a synchronization report.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PeerReport {
    pub(crate) machine: String,
    pub(crate) state: LinkState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Report {
    pub(crate) peers: Vec<PeerReport>,
}

impl Report {
    #[must_use]
    pub(crate) fn complete(&self) -> bool {
        self.peers
            .iter()
            .all(|peer| peer.state == LinkState::Synced)
    }
}

fn record(store: &Store, alias: &str, outcome: &Outcome) -> Result<PeerReport, SyncError> {
    let (state, detail) = match outcome {
        Outcome::Synced => (LinkState::Synced, None),
        Outcome::Partial => (LinkState::Deferred, Some("more events remain".to_owned())),
        Outcome::Deferred(rejection) => (LinkState::Deferred, Some(rejection.to_string())),
        Outcome::Failed(detail) => (LinkState::Failed, Some(detail.clone())),
    };
    store.record_link(
        alias,
        &Link {
            state,
            detail: detail.clone(),
            at: clock::now_millis(),
        },
    )?;
    Ok(PeerReport {
        machine: alias.to_owned(),
        state,
        detail,
    })
}

/// Synchronize with every pinned peer, or only `only`, retrying dependency waits while others progress.
pub(crate) fn sync_with<C: Channel>(
    store: &Store,
    only: Option<&str>,
    deadline: Deadline,
    connect: &mut impl FnMut(&str) -> Result<C, SyncError>,
) -> Result<Report, SyncError> {
    let peers = store.peers()?;
    if let Some(alias) = only
        && !peers.contains_key(alias)
    {
        return Err(SyncError::UnknownPeer(alias.to_owned()));
    }
    let mut pending: Vec<(String, Origin)> = peers
        .into_iter()
        .filter(|(alias, _)| only.is_none_or(|selected| selected == alias))
        .collect();
    let mut outcomes = Vec::new();
    let mut stored_any = false;
    for _ in 0..=pending.len() {
        let mut deferred = Vec::new();
        let mut progressed = false;
        for (alias, origin) in pending {
            let result = connect(&alias)
                .and_then(|mut channel| exchange(store, &origin, &mut channel, deadline));
            let outcome = match result {
                Ok((outcome, appended)) => {
                    progressed |= appended > 0;
                    outcome
                }
                Err(error) => Outcome::Failed(error.to_string()),
            };
            if matches!(outcome, Outcome::Deferred(_)) {
                deferred.push((alias.clone(), origin));
            }
            outcomes.push((alias, outcome));
        }
        stored_any |= progressed;
        if deferred.is_empty() || !progressed {
            break;
        }
        outcomes.retain(|(alias, _)| !deferred.iter().any(|(retry, _)| retry == alias));
        pending = deferred;
    }
    if stored_any {
        runner::dispatch(store)?;
    }
    let mut report = Report::default();
    for (alias, outcome) in outcomes {
        report.peers.push(record(store, &alias, &outcome)?);
    }
    report
        .peers
        .sort_by(|first, second| first.machine.cmp(&second.machine));
    Ok(report)
}

/// Synchronize over OpenSSH.
pub(crate) fn sync(
    store: &Store,
    only: Option<&str>,
    deadline: Deadline,
) -> Result<Report, SyncError> {
    sync_with(store, only, deadline, &mut |alias: &str| Ssh::new(alias))
}

/// Ask a machine for its chat identity.
pub(crate) fn identify(
    channel: &mut impl Channel,
    deadline: Deadline,
) -> Result<Origin, SyncError> {
    match channel.call(ChatRequest::Identity {}, deadline)? {
        ChatReply::Identity { origin } => Ok(origin),
        ChatReply::Exchanged(_)
        | ChatReply::Changed {}
        | ChatReply::Heartbeat {}
        | ChatReply::Rejected { .. } => Err(SyncError::Unexpected),
    }
}

/// Answer one chat request as this machine's node; a wait ends early once `abandoned` is set.
pub(crate) fn serve(
    store: &Store,
    request: ChatRequest,
    abandoned: &AtomicBool,
) -> Result<ChatReply, SyncError> {
    Ok(match request {
        ChatRequest::Identity {} => {
            store.publish_machine()?;
            ChatReply::Identity {
                origin: store.origin().clone(),
            }
        }
        ChatRequest::Exchange(offer) => {
            let reply = match store.respond(&offer)? {
                Ok(answer) => ChatReply::Exchanged(answer),
                Err(rejection) => ChatReply::Rejected { rejection },
            };
            runner::dispatch(store)?;
            reply
        }
        ChatRequest::Wait { to, seen, .. } if to == *store.origin() => wait(
            store,
            seen,
            Deadline::after_seconds(HEARTBEAT_SECONDS),
            abandoned,
        )?,
        ChatRequest::Wait { .. } => ChatReply::Rejected {
            rejection: Rejection::WrongPeer,
        },
    })
}

/// Hold until this machine authored an event after `seen`, the heartbeat, or the client leaving.
fn wait(
    store: &Store,
    seen: u64,
    deadline: Deadline,
    abandoned: &AtomicBool,
) -> Result<ChatReply, SyncError> {
    let mut pulse = Pulse::new(store, 2000)?;
    loop {
        if store.local_seen()? > seen {
            return Ok(ChatReply::Changed {});
        }
        if deadline.expired() || abandoned.load(Ordering::Acquire) {
            return Ok(ChatReply::Heartbeat {});
        }
        pulse.next(deadline)?;
    }
}

#[cfg(test)]
mod tests;
