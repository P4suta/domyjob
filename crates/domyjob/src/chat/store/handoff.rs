use domyjob_core::chat::event::{Body, Event};
use domyjob_core::chat::handoff::{Descriptor, Observation, Project, ReadLimit, Scope};
use domyjob_core::chat::id::{AgentId, EventId, Text};
use redb::{ReadableTable, WriteTransaction};
use serde::Serialize;

use super::tables::{
    HANDOFF_HEADERS, HANDOFF_READERS, HANDOFF_READS, Reader, delivery_key, encode, event_key,
    get_json, order_key,
};
use super::{StoreError, views};

pub(crate) const MAX_HANDOFFS: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct PendingObservation {
    descriptor: Descriptor,
    observation: Observation,
}

impl PendingObservation {
    pub(crate) const fn reader(&self) -> &AgentId {
        self.observation.reader()
    }

    pub(crate) fn commit(&self, tx: &mut super::Tx<'_>) -> Result<(), StoreError> {
        let actual = header(tx.transaction(), self.descriptor.id())?
            .ok_or_else(|| StoreError::Unknown(self.descriptor.id().to_string()))?;
        actual.authorize(self.reader(), &self.descriptor, ReadLimit::MAX)?;
        if observed(tx.transaction(), self.reader(), self.descriptor.id())? {
            return Ok(());
        }
        tx.author(
            domyjob_core::chat::policy::Priority::Ordinary,
            Body::HandoffRead {
                observation: self.observation.clone(),
                audience: self.descriptor.metadata().audience().clone(),
            },
        )?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ReadBody {
    pub(crate) descriptor: Descriptor,
    pub(crate) reader: AgentId,
    pub(crate) text: Text,
    #[serde(skip)]
    pending: PendingObservation,
}

impl ReadBody {
    pub(crate) const fn pending(&self) -> &PendingObservation {
        &self.pending
    }
}

fn prefix(reader: &AgentId, project: &Project, scope: &Scope) -> String {
    format!(
        "{reader}\u{1f}{}\u{1f}{}\u{1f}",
        project.line().as_str(),
        scope.line().as_str()
    )
}

pub(super) fn record(write: &WriteTransaction, event: &Event) -> Result<(), StoreError> {
    if let Body::Handoff { metadata, text, .. } = event.body() {
        let descriptor = Descriptor::new(event.id().clone(), metadata.clone(), text)?;
        let id = event_key(event.id());
        write
            .open_table(HANDOFF_HEADERS)?
            .insert(id.as_str(), encode(&descriptor)?.as_str())?;
        let mut readers = write.open_table(HANDOFF_READERS)?;
        for reader in metadata.permitted_readers().agents() {
            let key = format!(
                "{}{}",
                prefix(reader, metadata.project(), metadata.scope()),
                order_key(event)
            );
            readers.insert(key.as_str(), id.as_str())?;
        }
    }
    Ok(())
}

fn header(reader: &impl Reader, id: &EventId) -> Result<Option<Descriptor>, StoreError> {
    let found: Option<Descriptor> = get_json(&reader.table(HANDOFF_HEADERS)?, &event_key(id))?;
    if found
        .as_ref()
        .is_some_and(|descriptor| descriptor.id() != id)
    {
        return Err(StoreError::Corrupt("handoff header identity"));
    }
    Ok(found)
}

pub(crate) fn discover(
    store: &impl Reader,
    reader: &AgentId,
    context: (&Project, &Scope),
    limit: usize,
) -> Result<Vec<Descriptor>, StoreError> {
    if limit == 0 || limit > MAX_HANDOFFS {
        return Err(StoreError::Corrupt("handoff metadata limit"));
    }
    let start = prefix(reader, context.0, context.1);
    let end = format!("{start}\u{7f}");
    let index = store.table(HANDOFF_READERS)?;
    let mut descriptors = Vec::with_capacity(limit);
    for row in index.range(start.as_str()..end.as_str())?.rev().take(limit) {
        let (_key, value) = row?;
        let id = EventId::try_from(value.value().to_owned())?;
        let descriptor =
            header(store, &id)?.ok_or(StoreError::Corrupt("handoff header missing"))?;
        descriptor.authorize(reader, &descriptor, ReadLimit::MAX)?;
        if descriptor.metadata().project() != context.0
            || descriptor.metadata().scope() != context.1
        {
            return Err(StoreError::Corrupt("handoff metadata index"));
        }
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

fn load_using(
    descriptor: &Descriptor,
    actor: &AgentId,
    request: (&Descriptor, ReadLimit),
    fetch_body: impl FnOnce(&EventId) -> Result<Option<Event>, StoreError>,
) -> Result<ReadBody, StoreError> {
    let permit = descriptor.authorize(actor, request.0, request.1)?;
    let event = fetch_body(descriptor.id())?
        .ok_or_else(|| StoreError::Unknown(descriptor.id().to_string()))?;
    let Body::Handoff { metadata, text, .. } = event.body() else {
        return Err(StoreError::Corrupt("handoff body kind"));
    };
    permit.check_body(event.id(), metadata, text)?;
    Ok(ReadBody {
        descriptor: descriptor.clone(),
        reader: actor.clone(),
        text: text.clone(),
        pending: PendingObservation {
            descriptor: descriptor.clone(),
            observation: permit.observation(),
        },
    })
}

pub(crate) fn read(
    store: &impl Reader,
    actor: &AgentId,
    selected: &Descriptor,
    limit: ReadLimit,
) -> Result<ReadBody, StoreError> {
    let descriptor = header(store, selected.id())?
        .ok_or_else(|| StoreError::Unknown(selected.id().to_string()))?;
    load_using(&descriptor, actor, (selected, limit), |id| {
        views::event(store, id)
    })
}

pub(crate) fn observed(
    store: &impl Reader,
    actor: &AgentId,
    handoff: &EventId,
) -> Result<bool, StoreError> {
    Ok(store
        .table(HANDOFF_READS)?
        .get(delivery_key(actor, handoff).as_str())?
        .is_some())
}

pub(super) fn record_read(
    write: &WriteTransaction,
    actor: &AgentId,
    handoff: &EventId,
    observation: &EventId,
) -> Result<(), StoreError> {
    write.open_table(HANDOFF_READS)?.insert(
        delivery_key(actor, handoff).as_str(),
        event_key(observation).as_str(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
