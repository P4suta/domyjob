//! An in-memory ledger with the same admission and exchange rules as the durable store.
//!
//! Tests and fuzzing use it as the reference for convergence properties.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::convert::Infallible;

use super::card::{Card, MachineCard};
use super::event::{Body, Digest, Event, Sealed};
use super::exchange::{Outbox, Outgoing, Progress, accept, offer, offer_event, respond};
use super::id::{AgentId, EventId, Origin, RoomId};
use super::ledger::{Admitted, Change, Cursor, Failure, Ledger, Parent, Resolution, Room, append};

/// One machine's complete chat state held in memory.
#[derive(Debug, Clone)]
pub struct Model {
    origin: Origin,
    events: BTreeMap<EventId, Sealed>,
    cursors: BTreeMap<Origin, Cursor>,
    clock: u64,
    acks: BTreeMap<Origin, u64>,
    resolutions: BTreeMap<EventId, Resolution>,
    profiles: BTreeMap<AgentId, Box<Card>>,
    machines: BTreeMap<Origin, MachineCard>,
    rooms: BTreeMap<RoomId, Box<Room>>,
    open: BTreeSet<(AgentId, EventId)>,
    started: BTreeMap<EventId, AgentId>,
}

impl Model {
    #[must_use]
    pub const fn new(origin: Origin) -> Self {
        Self {
            origin,
            events: BTreeMap::new(),
            cursors: BTreeMap::new(),
            clock: 0,
            acks: BTreeMap::new(),
            resolutions: BTreeMap::new(),
            profiles: BTreeMap::new(),
            machines: BTreeMap::new(),
            rooms: BTreeMap::new(),
            open: BTreeSet::new(),
            started: BTreeMap::new(),
        }
    }

    /// Author the next local event.
    pub fn write(&mut self, body: Body) -> Result<Event, Failure<Infallible>> {
        append(self, body)
    }

    /// Every stored event in the shared display order.
    #[must_use]
    pub fn ordered(&self) -> Vec<&Event> {
        let mut events: Vec<&Event> = self.events.values().map(Sealed::event).collect();
        events.sort_by(|first, second| first.order().cmp(&second.order()));
        events
    }

    #[must_use]
    pub const fn resolutions(&self) -> &BTreeMap<EventId, Resolution> {
        &self.resolutions
    }

    #[must_use]
    pub const fn profiles(&self) -> &BTreeMap<AgentId, Box<Card>> {
        &self.profiles
    }

    #[must_use]
    pub const fn rooms(&self) -> &BTreeMap<RoomId, Box<Room>> {
        &self.rooms
    }

    #[must_use]
    pub const fn machines(&self) -> &BTreeMap<Origin, MachineCard> {
        &self.machines
    }

    /// Unresolved asks by responder.
    #[must_use]
    pub const fn open_asks(&self) -> &BTreeSet<(AgentId, EventId)> {
        &self.open
    }

    #[must_use]
    pub const fn started(&self) -> &BTreeMap<EventId, AgentId> {
        &self.started
    }

    /// Run exchange rounds with `peer` until neither side has more to send or a round is refused.
    ///
    /// Returns the progress of the last round, or the rejection that ended the exchange.
    pub fn sync_with(
        &mut self,
        peer: &mut Self,
        rounds: usize,
    ) -> Result<Progress, super::ledger::Rejection> {
        let mut last = None;
        for _ in 0..rounds {
            let Ok(request) = offer(self, &peer.origin);
            let request = request?;
            let Ok(answer) = respond(peer, &request);
            let Ok(progress) = accept(self, &request, answer?);
            let progress = progress?;
            let more = progress.more;
            last = Some(progress);
            if !more {
                break;
            }
        }
        last.ok_or(super::ledger::Rejection::Inconsistent)
    }
}

impl Ledger for Model {
    type Error = Infallible;

    fn local(&self) -> &Origin {
        &self.origin
    }

    fn cursor(&self, origin: &Origin) -> Result<Cursor, Infallible> {
        Ok(self.cursors.get(origin).copied().unwrap_or_default())
    }

    fn digest(&self, id: &EventId) -> Result<Option<Digest>, Infallible> {
        Ok(self.events.get(id).map(Sealed::digest))
    }

    fn parent(&self, id: &EventId) -> Result<Parent, Infallible> {
        Ok(match self.events.get(id).map(Sealed::event) {
            Some(event) if matches!(event.body(), Body::Omitted {}) => Parent::Opaque,
            Some(event) => Parent::Present(Box::new(event.clone())),
            None => Parent::Missing,
        })
    }

    fn resolution(&self, request: &EventId) -> Result<Option<Resolution>, Infallible> {
        Ok(self.resolutions.get(request).cloned())
    }

    fn clock(&self) -> Result<u64, Infallible> {
        Ok(self.clock)
    }

    fn has_room(&self) -> Result<bool, Infallible> {
        Ok(true)
    }

    fn commit(&mut self, sealed: &Sealed, admitted: Admitted) -> Result<(), Infallible> {
        let event = sealed.event();
        self.cursors.insert(event.origin().clone(), admitted.cursor);
        self.clock = self.clock.max(event.clock());
        self.events.insert(event.id().clone(), sealed.clone());
        match admitted.change {
            Change::Nothing => {}
            Change::Profile { agent, card } => match card {
                Some(card) => {
                    self.profiles.insert(agent, card);
                }
                None => {
                    self.profiles.remove(&agent);
                }
            },
            Change::Machine { card } => {
                self.machines.insert(event.origin().clone(), card);
            }
            Change::Room { id, room } => match room {
                Some(room) => {
                    self.rooms.insert(id, room);
                }
                None => {
                    self.rooms.remove(&id);
                }
            },
            Change::Ask { responder } => {
                self.open.insert((responder, event.id().clone()));
            }
            Change::Started { request, agent } => {
                self.started.insert(request, agent);
            }
            Change::Resolution {
                request,
                resolution,
                wins,
            } => {
                if wins {
                    self.open
                        .remove(&(resolution.responder.clone(), request.clone()));
                    self.resolutions.insert(request, *resolution);
                }
            }
        }
        Ok(())
    }
}

impl Outbox for Model {
    fn outgoing(&self, peer: &Origin, after: u64, batch: &mut Outgoing) -> Result<(), Infallible> {
        for sealed in self
            .events
            .values()
            .filter(|sealed| sealed.event().origin() == &self.origin)
            .filter(|sealed| sealed.event().id().seq().get() > after)
        {
            match offer_event(batch, sealed.event(), peer) {
                Ok(true) => {}
                Ok(false) | Err(_) => break,
            }
        }
        Ok(())
    }

    fn ack(&self, peer: &Origin) -> Result<u64, Infallible> {
        Ok(self.acks.get(peer).copied().unwrap_or(0))
    }

    fn record_ack(&mut self, peer: &Origin, seen: u64) -> Result<(), Infallible> {
        let local = self
            .cursors
            .get(&self.origin)
            .map_or(0, |cursor| cursor.seen);
        let ack = self.acks.entry(peer.clone()).or_insert(0);
        *ack = (*ack).max(seen.min(local));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
