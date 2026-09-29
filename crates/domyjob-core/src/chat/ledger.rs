use alloc::boxed::Box;

use serde::{Deserialize, Serialize};

use super::card::{Card, MachineCard};
use super::event::{Body, Event, Intent, Members, Outcome, Sealed};
use super::id::{AgentId, Audience, Conversation, EventId, Invalid, Line, Origin, RoomId};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub seen: u64,
    pub clock: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parent {
    Present(Box<Event>),
    Opaque,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "outcome")]
pub enum Ending {
    Answered,
    Ended(Outcome),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub event: EventId,
    pub clock: u64,
    pub ending: Ending,
    pub asker: AgentId,
    pub responder: AgentId,
}

impl Resolution {
    fn precedes(&self, other: &Self) -> bool {
        (self.clock, self.event.origin(), self.event.seq())
            < (other.clock, other.event.origin(), other.event.seq())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Room {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<Line<256>>,
    pub members: Members,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "reason")]
pub enum Rejection {
    #[error("{event} depends on {request}, which has not arrived yet")]
    MissingDependency { event: EventId, request: EventId },
    #[error("expected sequence {expected} from {origin}")]
    Gap { origin: Origin, expected: u64 },
    #[error("{event} conflicts with the stored event of the same ID")]
    Conflict { event: EventId },
    #[error("{event} does not advance its origin's clock")]
    ClockRegression { event: EventId },
    #[error("{event} does not match the request it answers")]
    Mismatch { event: EventId },
    #[error("{event} arrived from a machine that did not write it")]
    Unauthorized { event: EventId },
    #[error("the receiving ledger has reached its storage limit")]
    ResourceLimit,
    #[error("the request was meant for another machine")]
    WrongPeer,
    #[error("the peer claims history that does not exist here")]
    AheadOfHistory,
    #[error("the stored ledger is not consecutive")]
    Inconsistent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Nothing,
    Profile {
        agent: AgentId,
        card: Option<Box<Card>>,
    },
    Machine {
        card: MachineCard,
    },
    Room {
        id: RoomId,
        room: Option<Box<Room>>,
    },
    Ask {
        responder: AgentId,
    },
    Started {
        request: EventId,
        agent: AgentId,
    },
    Resolution {
        request: EventId,
        resolution: Box<Resolution>,
        wins: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub cursor: Cursor,
    pub change: Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Duplicate,
    Append(Admitted),
}

pub trait Ledger {
    type Error;

    fn local(&self) -> &Origin;
    fn cursor(&self, origin: &Origin) -> Result<Cursor, Self::Error>;
    fn digest(&self, id: &EventId) -> Result<Option<super::event::Digest>, Self::Error>;
    fn parent(&self, id: &EventId) -> Result<Parent, Self::Error>;
    fn resolution(&self, request: &EventId) -> Result<Option<Resolution>, Self::Error>;
    fn clock(&self) -> Result<u64, Self::Error>;
    fn has_room(&self) -> Result<bool, Self::Error>;
    fn commit(&mut self, sealed: &Sealed, admitted: Admitted) -> Result<(), Self::Error>;
}

#[derive(Debug, thiserror::Error)]
pub enum Failure<E> {
    #[error("chat storage failed")]
    Store(E),
    #[error(transparent)]
    Rejected(Rejection),
    #[error(transparent)]
    Invalid(Invalid),
}

fn same_thread(
    event: &Event,
    conversation: &Conversation,
    audience: &Audience,
    parent: &Event,
) -> bool {
    parent.body().conversation() == Some(conversation)
        && parent.body().audience() == Some(audience)
        && event.clock() > parent.clock()
}

fn ask_of(parent: &Event) -> Option<(AgentId, &AgentId)> {
    match parent.body() {
        Body::Message {
            from,
            intent: Intent::Ask { responder, .. },
            ..
        } => Some((
            AgentId::new(from.clone(), parent.origin().clone()),
            responder,
        )),
        Body::Message { .. }
        | Body::Profile { .. }
        | Body::Left { .. }
        | Body::Machine { .. }
        | Body::Room { .. }
        | Body::RoomClosed { .. }
        | Body::TurnStarted { .. }
        | Body::Resolved { .. }
        | Body::Omitted {} => None,
    }
}

struct Candidate {
    ending: Ending,
    author: AgentId,
}

impl Candidate {
    fn authorized(&self, asker: &AgentId, responder: &AgentId) -> bool {
        match self.ending {
            Ending::Ended(Outcome::Withdrawn) => self.author == *asker,
            Ending::Answered | Ending::Ended(_) => self.author == *responder,
        }
    }
}

fn resolve<L: Ledger + ?Sized>(
    ledger: &L,
    event: &Event,
    request: &EventId,
    candidate: &Candidate,
) -> Result<Change, Failure<L::Error>> {
    let mismatch = || {
        Failure::Rejected(Rejection::Mismatch {
            event: event.id().clone(),
        })
    };
    let (conversation, audience) = match (event.body().conversation(), event.body().audience()) {
        (Some(conversation), Some(audience)) => (conversation, audience),
        (None, _) | (_, None) => return Ok(Change::Nothing),
    };
    let current = ledger.resolution(request).map_err(Failure::Store)?;
    let (asker, responder) = match ledger.parent(request).map_err(Failure::Store)? {
        Parent::Missing => {
            return Err(Failure::Rejected(Rejection::MissingDependency {
                event: event.id().clone(),
                request: request.clone(),
            }));
        }
        Parent::Present(parent) => {
            if !same_thread(event, conversation, audience, &parent) {
                return Err(mismatch());
            }
            match ask_of(&parent) {
                Some((asker, responder)) => (asker, responder.clone()),
                None => return plain_reply(event, candidate.ending),
            }
        }
        Parent::Opaque => match &current {
            Some(known) => (known.asker.clone(), known.responder.clone()),
            None => return plain_reply(event, candidate.ending),
        },
    };
    if !candidate.authorized(&asker, &responder) {
        return match candidate.ending {
            Ending::Answered => Ok(Change::Nothing),
            Ending::Ended(_) => Err(mismatch()),
        };
    }
    let resolution = Resolution {
        event: event.id().clone(),
        clock: event.clock(),
        ending: candidate.ending,
        asker,
        responder,
    };
    let wins = current
        .as_ref()
        .is_none_or(|known| resolution.precedes(known));
    Ok(Change::Resolution {
        request: request.clone(),
        resolution: Box::new(resolution),
        wins,
    })
}

fn plain_reply<E>(event: &Event, ending: Ending) -> Result<Change, Failure<E>> {
    match ending {
        Ending::Answered => Ok(Change::Nothing),
        Ending::Ended(_) => Err(Failure::Rejected(Rejection::Mismatch {
            event: event.id().clone(),
        })),
    }
}

fn started<L: Ledger + ?Sized>(
    ledger: &L,
    event: &Event,
    request: &EventId,
) -> Result<Change, Failure<L::Error>> {
    let (Some(conversation), Some(audience), Some(agent)) = (
        event.body().conversation(),
        event.body().audience(),
        event.author(),
    ) else {
        return Ok(Change::Nothing);
    };
    match ledger.parent(request).map_err(Failure::Store)? {
        Parent::Missing => Err(Failure::Rejected(Rejection::MissingDependency {
            event: event.id().clone(),
            request: request.clone(),
        })),
        Parent::Opaque => Ok(Change::Nothing),
        Parent::Present(parent) => match ask_of(&parent) {
            Some((_, responder))
                if *responder == agent && same_thread(event, conversation, audience, &parent) =>
            {
                Ok(Change::Started {
                    request: request.clone(),
                    agent,
                })
            }
            Some(_) | None => Err(Failure::Rejected(Rejection::Mismatch {
                event: event.id().clone(),
            })),
        },
    }
}

fn change<L: Ledger + ?Sized>(ledger: &L, event: &Event) -> Result<Change, Failure<L::Error>> {
    let origin = event.origin();
    Ok(match event.body() {
        Body::Omitted {}
        | Body::Message {
            intent: Intent::Send {},
            ..
        } => Change::Nothing,
        Body::Profile { agent, card } => Change::Profile {
            agent: AgentId::new(agent.clone(), origin.clone()),
            card: Some(card.clone()),
        },
        Body::Left { agent } => Change::Profile {
            agent: AgentId::new(agent.clone(), origin.clone()),
            card: None,
        },
        Body::Machine { card } => Change::Machine { card: card.clone() },
        Body::Room {
            name,
            topic,
            members,
        } => Change::Room {
            id: RoomId::new(origin.clone(), name.clone()),
            room: Some(Box::new(Room {
                topic: topic.clone(),
                members: members.clone(),
            })),
        },
        Body::RoomClosed { name } => Change::Room {
            id: RoomId::new(origin.clone(), name.clone()),
            room: None,
        },
        Body::Message {
            intent: Intent::Ask { responder, .. },
            ..
        } => Change::Ask {
            responder: responder.clone(),
        },
        Body::Message {
            from,
            intent: Intent::Reply { request },
            ..
        } => {
            let author = AgentId::new(from.clone(), origin.clone());
            resolve(
                ledger,
                event,
                request,
                &Candidate {
                    ending: Ending::Answered,
                    author,
                },
            )?
        }
        Body::TurnStarted { request, .. } => started(ledger, event, request)?,
        Body::Resolved {
            request,
            agent,
            outcome,
            ..
        } => resolve(
            ledger,
            event,
            request,
            &Candidate {
                ending: Ending::Ended(*outcome),
                author: AgentId::new(agent.clone(), origin.clone()),
            },
        )?,
    })
}

pub fn admit<L: Ledger + ?Sized>(
    ledger: &L,
    sealed: &Sealed,
) -> Result<Decision, Failure<L::Error>> {
    let event = sealed.event();
    let cursor = ledger.cursor(event.origin()).map_err(Failure::Store)?;
    let seq = event.id().seq().get();
    if seq <= cursor.seen {
        return match ledger.digest(event.id()).map_err(Failure::Store)? {
            Some(stored) if stored == sealed.digest() => Ok(Decision::Duplicate),
            Some(_) | None => Err(Failure::Rejected(Rejection::Conflict {
                event: event.id().clone(),
            })),
        };
    }
    let expected = cursor
        .seen
        .checked_add(1)
        .ok_or(Failure::Invalid(Invalid("sequence exhausted")))?;
    if seq != expected {
        return Err(Failure::Rejected(Rejection::Gap {
            origin: event.origin().clone(),
            expected,
        }));
    }
    if event.clock() <= cursor.clock {
        return Err(Failure::Rejected(Rejection::ClockRegression {
            event: event.id().clone(),
        }));
    }
    Ok(Decision::Append(Admitted {
        cursor: Cursor {
            seen: seq,
            clock: event.clock(),
        },
        change: change(ledger, event)?,
    }))
}

fn commit<L: Ledger + ?Sized>(ledger: &mut L, sealed: &Sealed) -> Result<bool, Failure<L::Error>> {
    match admit(ledger, sealed)? {
        Decision::Duplicate => Ok(false),
        Decision::Append(admitted) => {
            ledger.commit(sealed, admitted).map_err(Failure::Store)?;
            Ok(true)
        }
    }
}

pub fn append<L: Ledger + ?Sized>(ledger: &mut L, body: Body) -> Result<Event, Failure<L::Error>> {
    let origin = ledger.local().clone();
    let cursor = ledger.cursor(&origin).map_err(Failure::Store)?;
    let exhausted = || Failure::Invalid(Invalid("local sequence or clock exhausted"));
    let seq = cursor
        .seen
        .checked_add(1)
        .and_then(core::num::NonZeroU64::new)
        .ok_or_else(exhausted)?;
    let clock = ledger
        .clock()
        .map_err(Failure::Store)?
        .max(cursor.clock)
        .checked_add(1)
        .filter(|clock| *clock < u64::MAX)
        .ok_or_else(exhausted)?;
    let event = Event::new(EventId::new(origin, seq), clock, body).map_err(Failure::Invalid)?;
    let sealed = Sealed::new(event).map_err(Failure::Invalid)?;
    if !commit(ledger, &sealed)? {
        return Err(Failure::Rejected(Rejection::Conflict {
            event: sealed.event().id().clone(),
        }));
    }
    Ok(sealed.into_event())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub seen: u64,
    pub appended: usize,
    pub rejected: Option<Rejection>,
}

pub fn receive<L: Ledger + ?Sized>(
    ledger: &mut L,
    origin: &Origin,
    events: &[Event],
) -> Result<Received, L::Error> {
    let mut appended = 0_usize;
    let mut rejected = None;
    for event in events {
        let outcome = if event.origin() != origin || origin == ledger.local() {
            Err(Failure::Rejected(Rejection::Unauthorized {
                event: event.id().clone(),
            }))
        } else if !ledger.has_room()? {
            Err(Failure::Rejected(Rejection::ResourceLimit))
        } else {
            Sealed::new(event.clone())
                .map_err(Failure::Invalid)
                .and_then(|sealed| commit(ledger, &sealed))
        };
        match outcome {
            Ok(true) => appended = appended.saturating_add(1),
            Ok(false) => {}
            Err(Failure::Store(error)) => return Err(error),
            Err(Failure::Rejected(rejection)) => {
                rejected = Some(rejection);
                break;
            }
            Err(Failure::Invalid(_)) => {
                rejected = Some(Rejection::Mismatch {
                    event: event.id().clone(),
                });
                break;
            }
        }
    }
    Ok(Received {
        seen: ledger.cursor(origin)?.seen,
        appended,
        rejected,
    })
}
