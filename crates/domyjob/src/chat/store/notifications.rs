use domyjob_core::chat::event::{Body, Event, Intent};
use domyjob_core::chat::id::{AgentId, Conversation, EventId, MAX_TEXT, Text};
use domyjob_core::chat::notification::{Delivery, Notice, QuestionState, escalation_due};
use domyjob_core::chat::policy::{Priority, audience};
use redb::ReadableTable;

use super::tables::{DELIVERY, ESCALATIONS, Reader, URGENT, decode, event_key, get_text};
use super::{Store, StoreError, Tx, views};

pub(crate) fn urgent(reader: &impl Reader, agent: &AgentId) -> Result<Vec<EventId>, StoreError> {
    let prefix = format!("{agent}\u{1f}");
    let mut messages = Vec::new();
    for entry in reader.table(URGENT)?.range(prefix.as_str()..)? {
        let (key, id) = entry?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        messages.push(EventId::try_from(id.value().to_owned())?);
    }
    Ok(messages)
}

pub(crate) fn delivered_to(
    reader: &impl Reader,
    agent: &AgentId,
) -> Result<Vec<EventId>, StoreError> {
    let prefix = format!("{agent}\u{1f}");
    let mut requests = Vec::new();
    for entry in reader.table(DELIVERY)?.range(prefix.as_str()..)? {
        let (key, stage) = entry?;
        let Some(request) = key.value().strip_prefix(&prefix) else {
            break;
        };
        if decode::<Delivery>(stage.value())? == Delivery::Delivered {
            requests.push(EventId::try_from(request.to_owned())?);
        }
    }
    Ok(requests)
}

pub(crate) fn escalation(
    reader: &impl Reader,
    request: &EventId,
) -> Result<Option<EventId>, StoreError> {
    get_text(&reader.table(ESCALATIONS)?, &event_key(request))?
        .map(EventId::try_from)
        .transpose()
        .map_err(StoreError::from)
}

impl Store {
    pub(crate) fn check_read_receipts(
        &self,
        agent: &AgentId,
        events: &[Event],
    ) -> Result<(), StoreError> {
        let _lock = crate::lock::OsLock::exclusive(&self.paths().lock())?;
        let database = super::open_database(self.paths())?;
        let write = database.begin_write()?;
        self.verify(&write)?;
        {
            let mut tx = Tx::new(&write, self.origin());
            for event in events {
                if matches!(event.body(), Body::Message { .. }) {
                    tx.receipt(agent, event, Delivery::Read)?;
                } else if !matches!(event.body(), Body::Resolved { .. }) {
                    return Err(StoreError::Invalid(domyjob_core::chat::id::Invalid(
                        "a read addresses a message or ask ending",
                    )));
                }
            }
        }
        write.abort()?;
        Ok(())
    }

    pub(crate) fn escalate(
        &self,
        now: domyjob_core::chat::event::Stamp,
    ) -> Result<usize, StoreError> {
        self.write(|tx| {
            let mut count = 0_usize;
            for (request, _) in views::open_asks(tx.transaction())? {
                if request.origin() != self.origin() {
                    continue;
                }
                let Some(parent) = views::event(tx.transaction(), &request)? else {
                    continue;
                };
                let Body::Message {
                    text,
                    at,
                    intent: Intent::Ask { chain, .. },
                    notice: Some(notice),
                    ..
                } = parent.body()
                else {
                    continue;
                };
                let Some(question) = &notice.question else {
                    continue;
                };
                let state = if views::resolution(tx.transaction(), &request)?.is_some() {
                    QuestionState::Answered
                } else if escalation(tx.transaction(), &request)?.is_some() {
                    QuestionState::Escalated
                } else {
                    QuestionState::Waiting
                };
                if !escalation_due(at.millis(), question.wait_millis(), now.millis(), state) {
                    continue;
                }
                let sender = parent
                    .author()
                    .ok_or(StoreError::Corrupt("a question has an author"))?;
                let fallback = question.fallback();
                let recipients =
                    domyjob_core::chat::event::Members::try_from(vec![fallback.clone()])?;
                let heading = format!(
                    "Fallback for unanswered question {request}.\nExpected: {}\n",
                    question.expected().as_str(),
                );
                let remaining = MAX_TEXT.saturating_sub(heading.len());
                let end = text
                    .as_str()
                    .floor_char_boundary(remaining.min(text.as_str().len()));
                let excerpt = text.as_str().get(..end).unwrap_or_default();
                let fallback_text = Text::try_from(format!("{heading}{excerpt}"))?;
                let event = tx.author(
                    Priority::Ending,
                    Body::Message {
                        conversation: Conversation::direct(&sender, fallback)?,
                        from: sender.name().clone(),
                        text: fallback_text,
                        audience: audience([&sender, fallback])?,
                        intent: Intent::Ask {
                            responder: fallback.clone(),
                            chain: chain.clone(),
                        },
                        at: now,
                        notice: Some(Notice {
                            recipients,
                            urgent: true,
                            subject: notice.subject.clone(),
                            question: None,
                        }),
                    },
                )?;
                tx.transaction()
                    .open_table(ESCALATIONS)?
                    .insert(event_key(&request).as_str(), event_key(event.id()).as_str())?;
                count = count.saturating_add(1);
            }
            Ok(count)
        })
    }
}

impl Tx<'_> {
    pub(crate) fn receipt(
        &mut self,
        agent: &AgentId,
        message: &Event,
        received: Delivery,
    ) -> Result<(), StoreError> {
        if agent.origin() != self.local_origin() {
            return Err(StoreError::Invalid(domyjob_core::chat::id::Invalid(
                "only a local recipient may record a receipt",
            )));
        }
        let current = views::delivery(self.transaction(), agent, message.id())?;
        if current.observe(received) == current {
            return Ok(());
        }
        let (conversation, audience) = message
            .body()
            .thread()
            .ok_or(StoreError::Corrupt("a receipt addresses a message"))?;
        self.author(
            Priority::Ending,
            Body::Receipt {
                request: message.id().clone(),
                conversation: conversation.clone(),
                audience: audience.clone(),
                agent: agent.name().clone(),
                delivery: received,
            },
        )?;
        Ok(())
    }
}
