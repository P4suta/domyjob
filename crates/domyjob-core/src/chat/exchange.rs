use alloc::vec::Vec;

use super::event::{Event, Sealed};
use super::id::Origin;
use super::ledger::{Ledger, Received, Rejection, receive};
use crate::chat_wire::{Answer, Batch, MAX_BATCH, Offer};

pub const BATCH_BYTES: usize = 896 * 1024;

pub trait Outbox: Ledger {
    fn outgoing(&self, peer: &Origin, after: u64, batch: &mut Outgoing) -> Result<(), Self::Error>;
    fn ack(&self, peer: &Origin) -> Result<u64, Self::Error>;
    fn record_ack(&mut self, peer: &Origin, seen: u64) -> Result<(), Self::Error>;
}

#[derive(Debug, Default)]
pub struct Outgoing {
    events: Vec<Event>,
    bytes: usize,
    more: bool,
}

impl Outgoing {
    pub fn offer(&mut self, sealed: Sealed) -> bool {
        let bytes = self
            .bytes
            .saturating_add(sealed.encoded().len())
            .saturating_add(1);
        if self.events.len() >= MAX_BATCH || (!self.events.is_empty() && bytes > BATCH_BYTES) {
            self.more = true;
            return false;
        }
        self.bytes = bytes;
        self.events.push(sealed.into_event());
        true
    }

    fn into_batch(self) -> Result<Batch, super::id::Invalid> {
        Batch::new(self.events, self.more)
    }
}

pub fn offer_event(
    batch: &mut Outgoing,
    event: &Event,
    peer: &Origin,
) -> Result<bool, super::id::Invalid> {
    Ok(batch.offer(Sealed::new(event.export(peer))?))
}

fn outgoing<L: Outbox + ?Sized>(
    ledger: &L,
    peer: &Origin,
    after: u64,
) -> Result<Result<Batch, Rejection>, L::Error> {
    let mut batch = Outgoing::default();
    ledger.outgoing(peer, after, &mut batch)?;
    Ok(batch
        .into_batch()
        .map_err(|_invalid| Rejection::Inconsistent))
}

pub fn offer<L: Outbox + ?Sized>(
    ledger: &L,
    peer: &Origin,
) -> Result<Result<Offer, Rejection>, L::Error> {
    let after = ledger.ack(peer)?;
    let seen = ledger.cursor(peer)?.seen;
    Ok(outgoing(ledger, peer, after)?.map(|batch| Offer {
        from: ledger.local().clone(),
        to: peer.clone(),
        after,
        seen,
        batch,
    }))
}

pub fn respond<L: Outbox + ?Sized>(
    ledger: &mut L,
    offer: &Offer,
) -> Result<Result<Answer, Rejection>, L::Error> {
    if offer.to != *ledger.local() || offer.from == *ledger.local() {
        return Ok(Err(Rejection::WrongPeer));
    }
    if offer.seen > ledger.cursor(ledger.local())?.seen {
        return Ok(Err(Rejection::AheadOfHistory));
    }
    let received = receive(ledger, &offer.from, offer.batch.events())?;
    ledger.record_ack(&offer.from, offer.seen)?;
    Ok(
        outgoing(ledger, &offer.from, offer.seen)?.map(|batch| Answer {
            from: ledger.local().clone(),
            seen: received.seen,
            batch,
            rejected: received.rejected,
        }),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub received: Received,
    pub refused: Option<Rejection>,
    pub more: bool,
}

pub fn accept<L: Outbox + ?Sized>(
    ledger: &mut L,
    offer: &Offer,
    answer: Answer,
) -> Result<Result<Progress, Rejection>, L::Error> {
    if answer.from != offer.to {
        return Ok(Err(Rejection::WrongPeer));
    }
    if answer.seen > ledger.cursor(ledger.local())?.seen {
        return Ok(Err(Rejection::AheadOfHistory));
    }
    let received = receive(ledger, &answer.from, answer.batch.events())?;
    ledger.record_ack(&answer.from, answer.seen)?;
    let sent_all = answer.rejected.is_none();
    let got_all = received.rejected.is_none();
    Ok(Ok(Progress {
        more: (offer.batch.more() && sent_all) || (answer.batch.more() && got_all),
        refused: answer.rejected,
        received,
    }))
}
