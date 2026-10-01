use domyjob_core::chat::card::{Card, Mode};
use domyjob_core::chat::event::{Body, Event, Intent, Outcome};
use domyjob_core::chat::id::{AgentId, EventId, Origin, Text};
use domyjob_core::chat::policy::Priority;
use redb::ReadableTable;

use super::tables::{LOCAL_AGENTS, Reader, TURNS, encode, event_key};
use super::views::{self, LocalAgent};
use super::{Store, StoreError, Tx};

#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub(crate) request: Event,
    pub(crate) agent: AgentId,
    pub(crate) card: Card,
    pub(crate) config: LocalAgent,
}

#[derive(Debug)]
pub(crate) enum Completion {
    Answered { text: Text, session: String },
    Failed,
}

fn ending(request: &Event, by: &AgentId, outcome: Outcome) -> Result<Body, StoreError> {
    let (conversation, audience) = request
        .body()
        .thread()
        .ok_or(StoreError::Corrupt("an ask is a message"))?;
    Ok(Body::Resolved {
        request: request.id().clone(),
        conversation: conversation.clone(),
        audience: audience.clone(),
        agent: by.name().clone(),
        outcome,
    })
}

impl Tx<'_> {
    pub(crate) fn end(
        &mut self,
        request: &Event,
        by: &AgentId,
        outcome: Outcome,
    ) -> Result<(), StoreError> {
        if views::resolution(self.transaction(), request.id())?.is_some() {
            return Ok(());
        }
        self.author(Priority::Ending, ending(request, by, outcome)?)?;
        Ok(())
    }

    fn open_for(&self, agent: &AgentId) -> Result<Vec<Event>, StoreError> {
        let claimed = self.transaction().open_table(TURNS)?;
        let mut asks = Vec::new();
        for (request, responder) in views::open_asks(self.transaction())? {
            if responder == *agent
                && claimed.get(event_key(&request).as_str())?.is_none()
                && let Some(event) = views::event(self.transaction(), &request)?
            {
                asks.push(event);
            }
        }
        asks.sort_by(|first, second| first.order().cmp(&second.order()));
        Ok(asks)
    }
}

#[derive(Debug, Default)]
struct Triage {
    ready: Vec<AgentId>,
    unavailable: Vec<(EventId, AgentId)>,
}

fn triage(reader: &impl Reader, origin: &Origin) -> Result<Triage, StoreError> {
    let mut found = Triage::default();
    for (request, responder) in views::open_asks(reader)? {
        if responder.origin() != origin {
            continue;
        }
        let card = views::profile(reader, &responder)?;
        let config = views::agent_config(reader, responder.name())?;
        match (card, config) {
            (Some(card), Some(_)) if card.mode == Mode::Managed => {
                if !found.ready.contains(&responder) {
                    found.ready.push(responder);
                }
            }
            (Some(_), Some(_)) => {}
            (None, _) | (Some(_), None) => found.unavailable.push((request, responder)),
        }
    }
    Ok(found)
}

impl Store {
    pub(crate) fn dispatchable(&self) -> Result<Vec<AgentId>, StoreError> {
        let seen = self.read(|read| triage(read, self.origin()))?;
        if seen.unavailable.is_empty() {
            return Ok(seen.ready);
        }
        self.write(|tx| {
            let current = triage(tx.transaction(), self.origin())?;
            for (request, responder) in current.unavailable {
                if let Some(event) = views::event(tx.transaction(), &request)? {
                    tx.end(&event, &responder, Outcome::Unavailable)?;
                }
            }
            Ok(current.ready)
        })
    }

    pub(crate) fn recover(&self, agent: &AgentId) -> Result<usize, StoreError> {
        self.write(|tx| {
            let mut claimed = Vec::new();
            for entry in tx.transaction().open_table(TURNS)?.iter()? {
                let (request, owner) = entry?;
                if owner.value() == agent.to_string() {
                    claimed.push(EventId::try_from(request.value().to_owned())?);
                }
            }
            for request in &claimed {
                if let Some(event) = views::event(tx.transaction(), request)? {
                    tx.end(&event, agent, Outcome::Interrupted)?;
                }
                tx.transaction()
                    .open_table(TURNS)?
                    .remove(event_key(request).as_str())?;
            }
            Ok(claimed.len())
        })
    }

    pub(crate) fn claim(&self, agent: &AgentId) -> Result<Option<Turn>, StoreError> {
        self.write(|tx| {
            let Some(request) = tx.open_for(agent)?.into_iter().next() else {
                return Ok(None);
            };
            let card = views::profile(tx.transaction(), agent)?
                .ok_or_else(|| StoreError::Unknown(agent.to_string()))?;
            let config = views::agent_config(tx.transaction(), agent.name())?
                .ok_or_else(|| StoreError::Unknown(agent.to_string()))?;
            let (conversation, audience) = request
                .body()
                .thread()
                .ok_or(StoreError::Corrupt("an ask is a message"))?;
            tx.author(
                Priority::Ending,
                Body::TurnStarted {
                    request: request.id().clone(),
                    conversation: conversation.clone(),
                    audience: audience.clone(),
                    agent: agent.name().clone(),
                },
            )?;
            tx.transaction()
                .open_table(TURNS)?
                .insert(event_key(request.id()).as_str(), agent.to_string().as_str())?;
            Ok(Some(Turn {
                request,
                agent: agent.clone(),
                card,
                config,
            }))
        })
    }

    pub(crate) fn finish(&self, turn: &Turn, completion: Completion) -> Result<(), StoreError> {
        self.write(|tx| {
            match completion {
                Completion::Answered { text, session } => {
                    let (conversation, audience) = turn
                        .request
                        .body()
                        .thread()
                        .ok_or(StoreError::Corrupt("an ask is a message"))?;
                    tx.author(
                        Priority::Ending,
                        Body::Message {
                            conversation: conversation.clone(),
                            from: turn.agent.name().clone(),
                            text,
                            audience: audience.clone(),
                            intent: Intent::Reply {
                                request: turn.request.id().clone(),
                            },
                            at: crate::platform::clock::stamp(),
                        },
                    )?;
                    let config = LocalAgent {
                        session: Some(session),
                        ..turn.config.clone()
                    };
                    tx.transaction()
                        .open_table(LOCAL_AGENTS)?
                        .insert(turn.agent.name().as_str(), encode(&config)?.as_str())?;
                }
                Completion::Failed => tx.end(&turn.request, &turn.agent, Outcome::Failed)?,
            }
            tx.transaction()
                .open_table(TURNS)?
                .remove(event_key(turn.request.id()).as_str())?;
            Ok(())
        })
    }
}
