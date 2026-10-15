use domyjob_core::chat::event::{Body, Event};
use domyjob_core::chat::id::AgentId;
use domyjob_core::chat::notification::Delivery;

use super::{OpsError, Session};
use crate::chat::store::{self, StoreError};

#[derive(Debug, Clone)]
pub(crate) struct PendingRead {
    reader: AgentId,
    events: Vec<Event>,
    advance_cursor: bool,
}

impl PendingRead {
    pub(super) fn prepare(
        session: &Session,
        events: Vec<Event>,
        advance_cursor: bool,
    ) -> Result<Self, OpsError> {
        let reader = session.me()?.clone();
        if !advance_cursor
            && events
                .iter()
                .any(|event| !matches!(event.body(), Body::Message { .. }))
        {
            return Err(OpsError::Usage("a full-body read addresses a message"));
        }
        session.store.check_read_receipts(&reader, &events)?;
        Ok(Self {
            reader,
            events,
            advance_cursor,
        })
    }

    pub(super) fn events(&self) -> &[Event] {
        &self.events
    }

    pub(super) fn unread(&self, session: &Session) -> Result<usize, OpsError> {
        Ok(session.store.read(|read| {
            let inspected = super::MAX_LIMIT.saturating_add(self.events.len());
            Ok(store::unread(read, &self.reader, inspected)?
                .iter()
                .filter(|event| {
                    !self
                        .events
                        .iter()
                        .any(|selected| selected.id() == event.id())
                })
                .take(super::MAX_LIMIT)
                .count())
        })?)
    }
}

pub(crate) fn complete_read(session: &Session, pending: &PendingRead) -> Result<(), OpsError> {
    if session.me()? != &pending.reader {
        return Err(OpsError::Usage(
            "the delivered messages belong to another acting identity",
        ));
    }
    session.store.write(|tx| {
        for selected in &pending.events {
            let actual = store::event(tx.transaction(), selected.id())?
                .ok_or_else(|| StoreError::Unknown(selected.id().to_string()))?;
            if &actual != selected {
                return Err(StoreError::Corrupt(
                    "the selected message changed before read completion",
                ));
            }
            if matches!(actual.body(), Body::Message { .. }) {
                tx.receipt(&pending.reader, &actual, Delivery::Read)?;
            }
        }
        if pending.advance_cursor
            && let Some(last) = pending.events.last()
        {
            tx.mark_read(&pending.reader, last)?;
        }
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
