//! Managed turns: claim the next ask, finish it, and recover abandoned claims, each atomically.

use domyjob_core::chat::card::Card;
use domyjob_core::chat::event::{Body, Event, Intent, Outcome};
use domyjob_core::chat::id::{AgentId, EventId, Text};
use domyjob_core::chat::policy::Priority;
use redb::ReadableTable;

use super::tables::{LOCAL_AGENTS, TURNS, encode, event_key};
use super::views::{self, LocalAgent};
use super::{Store, StoreError, Tx};

/// An ask claimed by its local responder, with everything needed to run it.
#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub(crate) request: Event,
    pub(crate) agent: AgentId,
    pub(crate) card: Card,
    pub(crate) config: LocalAgent,
}

/// How a managed turn ended.
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
    /// End an ask with `outcome` unless it already has an ending.
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

impl Store {
    /// Resolve asks addressed to local agents that are unknown here or not managed by this machine.
    ///
    /// Returns the managed agents that have waiting asks.
    pub(crate) fn dispatchable(&self) -> Result<Vec<AgentId>, StoreError> {
        self.write(|tx| {
            let mut ready = Vec::new();
            for (request, responder) in views::open_asks(tx.transaction())? {
                if responder.origin() != self.origin() {
                    continue;
                }
                let card = views::profile(tx.transaction(), &responder)?;
                let config = views::agent_config(tx.transaction(), responder.name())?;
                match (card, config) {
                    (Some(card), Some(_))
                        if card.mode == domyjob_core::chat::card::Mode::Managed =>
                    {
                        if !ready.contains(&responder) {
                            ready.push(responder);
                        }
                    }
                    (Some(_), Some(_)) => {}
                    (None, _) | (Some(_), None) => {
                        if let Some(event) = views::event(tx.transaction(), &request)? {
                            tx.end(&event, &responder, Outcome::Unavailable)?;
                        }
                    }
                }
            }
            Ok(ready)
        })
    }

    /// Record every abandoned claim of `agent` as interrupted; the caller holds the agent's lock.
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

    /// Claim the oldest unresolved ask of `agent` and announce that its turn started.
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

    /// Store a turn's result, its session, and the end of its claim in one transaction.
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
                            at: crate::platform::clock::now_millis(),
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
