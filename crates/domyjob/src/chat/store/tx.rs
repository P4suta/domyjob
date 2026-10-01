use domyjob_core::chat::event::{Body, Digest, Event, Sealed};
use domyjob_core::chat::exchange::{Outbox, Outgoing, offer_event};
use domyjob_core::chat::id::{Conversation, EventId, Origin};
use domyjob_core::chat::ledger::{Admitted, Change, Cursor, Ledger, Parent, Resolution};
use domyjob_core::chat::policy;
use redb::WriteTransaction;

use super::StoreError;
use super::tables::{
    self, ACKS, COUNTERS, CURSORS, EVENTS, JOINED, MACHINES, OPEN, ORDER, PROFILES, RESOLUTIONS,
    ROOMS, STARTED, THREADS, encode, event_key, get_number, joined_key, order_key, room_key,
    thread_key,
};
use super::views::{self, Stored};

pub(crate) struct Tx<'a> {
    write: &'a WriteTransaction,
    origin: &'a Origin,
    appended: bool,
}

impl<'a> Tx<'a> {
    pub(super) const fn new(write: &'a WriteTransaction, origin: &'a Origin) -> Self {
        Self {
            write,
            origin,
            appended: false,
        }
    }

    pub(crate) const fn transaction(&self) -> &'a WriteTransaction {
        self.write
    }

    pub(super) const fn appended(&self) -> bool {
        self.appended
    }

    pub(super) fn generation(&self) -> Result<u64, StoreError> {
        get_number(&self.write.open_table(COUNTERS)?, "generation")
    }
}

fn bump(write: &WriteTransaction, key: &str, amount: u64) -> Result<u64, StoreError> {
    let mut counters = write.open_table(COUNTERS)?;
    let value = get_number(&counters, key)?
        .checked_add(amount)
        .ok_or(StoreError::Corrupt("chat counter overflow"))?;
    counters.insert(key, value)?;
    Ok(value)
}

fn record(write: &WriteTransaction, sealed: &Sealed, cursor: Cursor) -> Result<(), StoreError> {
    let event = sealed.event();
    let id = event_key(event.id());
    let order = order_key(event);
    write
        .open_table(EVENTS)?
        .insert(id.as_str(), sealed.encoded())?;
    write
        .open_table(ORDER)?
        .insert(order.as_str(), id.as_str())?;
    if let Some(conversation) = event.body().conversation() {
        write
            .open_table(THREADS)?
            .insert(thread_key(conversation, &order).as_str(), id.as_str())?;
        if let Conversation::Direct(pair) = conversation {
            let mut joined = write.open_table(JOINED)?;
            for agent in pair.agents() {
                joined.insert(joined_key(agent, conversation).as_str(), "")?;
            }
        }
    }
    write
        .open_table(CURSORS)?
        .insert(event.origin().as_str(), (cursor.seen, cursor.clock))?;
    let clock = get_number(&write.open_table(COUNTERS)?, "clock")?.max(event.clock());
    write.open_table(COUNTERS)?.insert("clock", clock)?;
    bump(write, "events", 1)?;
    bump(
        write,
        "bytes",
        u64::try_from(sealed.encoded().len()).map_err(|_size| StoreError::Corrupt("event size"))?,
    )?;
    bump(write, "generation", 1)?;
    Ok(())
}

fn upsert<T: serde::Serialize>(
    write: &WriteTransaction,
    table: tables::TextTable,
    key: &str,
    value: Option<&T>,
) -> Result<(), StoreError> {
    let mut table = write.open_table(table)?;
    match value {
        Some(value) => {
            table.insert(key, encode(value)?.as_str())?;
        }
        None => {
            table.remove(key)?;
        }
    }
    Ok(())
}

fn project(write: &WriteTransaction, event: &Event, change: Change) -> Result<(), StoreError> {
    match change {
        Change::Nothing => {}
        Change::Profile { agent, card } => {
            upsert(write, PROFILES, &agent.to_string(), card.as_deref())?;
        }
        Change::Machine { card } => {
            upsert(write, MACHINES, event.origin().as_str(), Some(&card))?;
        }
        Change::Room { id, room } => upsert(write, ROOMS, &room_key(&id), room.as_deref())?,
        Change::Ask { responder } => {
            write.open_table(OPEN)?.insert(
                event_key(event.id()).as_str(),
                responder.to_string().as_str(),
            )?;
        }
        Change::Started { request, agent } => {
            write
                .open_table(STARTED)?
                .insert(event_key(&request).as_str(), agent.to_string().as_str())?;
        }
        Change::Resolution {
            request,
            resolution,
            wins,
        } => {
            if wins {
                let key = event_key(&request);
                upsert(write, RESOLUTIONS, &key, Some(resolution.as_ref()))?;
                write.open_table(OPEN)?.remove(key.as_str())?;
                write.open_table(STARTED)?.remove(key.as_str())?;
            }
        }
    }
    Ok(())
}

impl Ledger for Tx<'_> {
    type Error = StoreError;

    fn local(&self) -> &Origin {
        self.origin
    }

    fn cursor(&self, origin: &Origin) -> Result<Cursor, StoreError> {
        views::cursor(self.write, origin)
    }

    fn digest(&self, id: &EventId) -> Result<Option<Digest>, StoreError> {
        Ok(views::stored(self.write, id)?.map(|stored| match stored {
            Stored::Event { encoded, .. } => Digest::of(encoded.as_bytes()),
            Stored::Cleaned { digest, .. } => digest,
        }))
    }

    fn parent(&self, id: &EventId) -> Result<Parent, StoreError> {
        Ok(match views::stored(self.write, id)? {
            Some(Stored::Event { event, .. }) if !matches!(event.body(), Body::Omitted {}) => {
                Parent::Present(event)
            }
            Some(Stored::Event { .. } | Stored::Cleaned { .. }) => Parent::Opaque,
            None => Parent::Missing,
        })
    }

    fn resolution(&self, request: &EventId) -> Result<Option<Resolution>, StoreError> {
        views::resolution(self.write, request)
    }

    fn clock(&self) -> Result<u64, StoreError> {
        get_number(&self.write.open_table(COUNTERS)?, "clock")
    }

    fn has_room(&self) -> Result<bool, StoreError> {
        Ok(policy::fits_received(views::usage(self.write)?))
    }

    fn commit(&mut self, sealed: &Sealed, admitted: Admitted) -> Result<(), StoreError> {
        record(self.write, sealed, admitted.cursor)?;
        project(self.write, sealed.event(), admitted.change)?;
        self.appended = true;
        Ok(())
    }
}

impl Outbox for Tx<'_> {
    fn outgoing(&self, peer: &Origin, after: u64, batch: &mut Outgoing) -> Result<(), StoreError> {
        let seen = views::cursor(self.write, self.origin)?.seen;
        let mut seq = after;
        while seq < seen {
            seq = seq
                .checked_add(1)
                .ok_or(StoreError::Corrupt("sequence overflow"))?;
            let id = EventId::new(
                self.origin.clone(),
                std::num::NonZeroU64::new(seq).ok_or(StoreError::Corrupt("zero sequence"))?,
            );
            let offered = match views::stored(self.write, &id)? {
                Some(Stored::Event { event, .. }) => offer_event(batch, &event, peer)?,
                Some(Stored::Cleaned { clock, .. }) => {
                    let placeholder = Event::new(id, clock, Body::Omitted {})?;
                    batch.offer(Sealed::new(placeholder)?)
                }
                None => return Err(StoreError::Corrupt("local sequence has a gap")),
            };
            if !offered {
                break;
            }
        }
        Ok(())
    }

    fn ack(&self, peer: &Origin) -> Result<u64, StoreError> {
        get_number(&self.write.open_table(ACKS)?, peer.as_str())
    }

    fn record_ack(&mut self, peer: &Origin, seen: u64) -> Result<(), StoreError> {
        let local = views::cursor(self.write, self.origin)?.seen;
        let mut acks = self.write.open_table(ACKS)?;
        let current = get_number(&acks, peer.as_str())?;
        let next = current.max(seen.min(local));
        if next != current {
            acks.insert(peer.as_str(), next)?;
        }
        Ok(())
    }
}
