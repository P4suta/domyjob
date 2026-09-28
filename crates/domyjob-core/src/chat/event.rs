use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use super::card::{Card, MachineCard};
use super::id::{
    AgentId, AgentName, Audience, Conversation, EventId, Invalid, Line, Origin, RoomName, Text,
};

/// The encoded size limit of one event, leaving room for a batch inside a 1 MiB frame.
pub const MAX_EVENT_BYTES: usize = 400 * 1024;
/// The deepest chain of agents that may wait on each other through nested asks.
pub const MAX_CHAIN: usize = 8;

macro_rules! agent_list {
    ($(#[$doc:meta])* $name:ident, $min:literal, $max:expr, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "Vec<AgentId>", into = "Vec<AgentId>")]
        pub struct $name(Vec<AgentId>);

        impl TryFrom<Vec<AgentId>> for $name {
            type Error = Invalid;

            fn try_from(agents: Vec<AgentId>) -> Result<Self, Self::Error> {
                if !($min..=$max).contains(&agents.len()) {
                    return Err(Invalid($what));
                }
                let mut sorted: Vec<&AgentId> = agents.iter().collect();
                sorted.sort();
                if sorted.windows(2).any(|pair| matches!(pair, [first, second] if first == second)) {
                    return Err(Invalid($what));
                }
                Ok(Self(agents))
            }
        }

        impl From<$name> for Vec<AgentId> {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl $name {
            #[must_use]
            pub fn agents(&self) -> &[AgentId] {
                &self.0
            }

            #[must_use]
            pub fn contains(&self, agent: &AgentId) -> bool {
                self.0.contains(agent)
            }

            #[must_use]
            pub const fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }
    };
}

agent_list!(
    /// Agents whose turns already wait on an ask, outermost first; the sender is implicit.
    Chain,
    0,
    MAX_CHAIN,
    "delegation chain"
);
agent_list!(
    /// The unique agents of a room.
    Members,
    1,
    64,
    "room members"
);

impl Chain {
    /// The chain carried by an ask sent while `waiting` is blocked on the turn of this chain.
    pub fn extended(&self, waiting: AgentId) -> Result<Self, Invalid> {
        let mut agents = self.0.clone();
        agents.push(waiting);
        Self::try_from(agents)
    }
}

/// What a message asks of its audience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Intent {
    Send {},
    Ask {
        responder: AgentId,
        #[serde(default, skip_serializing_if = "Chain::is_empty")]
        chain: Chain,
    },
    Reply {
        request: EventId,
    },
}

/// A terminal state of an ask other than an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Failed,
    Interrupted,
    Unavailable,
    Withdrawn,
}

impl Outcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Unavailable => "unavailable",
            Self::Withdrawn => "withdrawn",
        }
    }
}

/// The content of an event; every agent or room it names belongs to the event's origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum Body {
    Profile {
        agent: AgentName,
        card: Box<Card>,
    },
    Left {
        agent: AgentName,
    },
    Machine {
        card: MachineCard,
    },
    Room {
        name: RoomName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        topic: Option<Line<256>>,
        members: Members,
    },
    RoomClosed {
        name: RoomName,
    },
    Message {
        conversation: Conversation,
        from: AgentName,
        text: Text,
        audience: Audience,
        intent: Intent,
        at: u64,
    },
    TurnStarted {
        request: EventId,
        conversation: Conversation,
        audience: Audience,
        agent: AgentName,
    },
    Resolved {
        request: EventId,
        conversation: Conversation,
        audience: Audience,
        agent: AgentName,
        outcome: Outcome,
    },
    Omitted {},
}

impl Body {
    /// The conversation and audience of a message or of an ask's progress.
    #[must_use]
    pub const fn thread(&self) -> Option<(&Conversation, &Audience)> {
        match self {
            Self::Message {
                conversation,
                audience,
                ..
            }
            | Self::TurnStarted {
                conversation,
                audience,
                ..
            }
            | Self::Resolved {
                conversation,
                audience,
                ..
            } => Some((conversation, audience)),
            Self::Profile { .. }
            | Self::Left { .. }
            | Self::Machine { .. }
            | Self::Room { .. }
            | Self::RoomClosed { .. }
            | Self::Omitted {} => None,
        }
    }

    #[must_use]
    pub fn audience(&self) -> Option<&Audience> {
        self.thread().map(|(_, audience)| audience)
    }

    #[must_use]
    pub fn conversation(&self) -> Option<&Conversation> {
        self.thread().map(|(conversation, _)| conversation)
    }

    /// The earlier event this one depends on.
    #[must_use]
    pub const fn request(&self) -> Option<&EventId> {
        if let Self::Message {
            intent: Intent::Reply { request },
            ..
        }
        | Self::TurnStarted { request, .. }
        | Self::Resolved { request, .. } = self
        {
            Some(request)
        } else {
            None
        }
    }

    /// The local agent that authored the event, if one did.
    #[must_use]
    pub const fn author(&self) -> Option<&AgentName> {
        match self {
            Self::Message { from: agent, .. }
            | Self::Profile { agent, .. }
            | Self::Left { agent }
            | Self::TurnStarted { agent, .. }
            | Self::Resolved { agent, .. } => Some(agent),
            Self::Machine { .. }
            | Self::Room { .. }
            | Self::RoomClosed { .. }
            | Self::Omitted {} => None,
        }
    }
}

/// One validated entry of an origin's ordered ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawEvent", into = "RawEvent")]
pub struct Event {
    id: EventId,
    clock: u64,
    body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvent {
    origin: Origin,
    seq: NonZeroU64,
    clock: u64,
    body: Body,
}

impl TryFrom<RawEvent> for Event {
    type Error = Invalid;

    fn try_from(raw: RawEvent) -> Result<Self, Self::Error> {
        Self::new(EventId::new(raw.origin, raw.seq), raw.clock, raw.body)
    }
}

impl From<Event> for RawEvent {
    fn from(event: Event) -> Self {
        Self {
            seq: event.id.seq(),
            origin: event.id.origin().clone(),
            clock: event.clock,
            body: event.body,
        }
    }
}

fn valid_ask(
    sender: &AgentId,
    conversation: &Conversation,
    audience: &Audience,
    (responder, chain): (&AgentId, &Chain),
) -> bool {
    responder != sender
        && !chain.contains(responder)
        && !chain.contains(sender)
        && audience.includes(responder.origin())
        && match conversation {
            Conversation::Direct(pair) => pair.other(sender) == Some(responder),
            Conversation::Room(_) => true,
        }
}

/// A direct conversation belongs to exactly its two agents and their machines.
fn valid_thread(conversation: &Conversation, audience: &Audience, author: &AgentId) -> bool {
    match conversation {
        Conversation::Direct(pair) => {
            let mut machines: Vec<&Origin> =
                pair.agents().iter().map(|agent| agent.origin()).collect();
            machines.sort();
            machines.dedup();
            pair.includes(author) && audience.members().iter().eq(machines)
        }
        Conversation::Room(_) => true,
    }
}

impl Event {
    pub fn new(id: EventId, clock: u64, body: Body) -> Result<Self, Invalid> {
        if clock == 0 || clock == u64::MAX {
            return Err(Invalid("event clock"));
        }
        if let Some((conversation, audience)) = body.thread() {
            let author = body
                .author()
                .map(|name| AgentId::new(name.clone(), id.origin().clone()));
            if !audience.includes(id.origin())
                || !author.is_some_and(|author| valid_thread(conversation, audience, &author))
            {
                return Err(Invalid("conversation author or audience"));
            }
        }
        if let Body::Message {
            conversation,
            from,
            audience,
            intent: Intent::Ask { responder, chain },
            ..
        } = &body
        {
            let sender = AgentId::new(from.clone(), id.origin().clone());
            if !valid_ask(&sender, conversation, audience, (responder, chain)) {
                return Err(Invalid("ask responder, conversation, or delegation chain"));
            }
        }
        Ok(Self { id, clock, body })
    }

    #[must_use]
    pub const fn id(&self) -> &EventId {
        &self.id
    }

    #[must_use]
    pub const fn origin(&self) -> &Origin {
        self.id.origin()
    }

    #[must_use]
    pub const fn clock(&self) -> u64 {
        self.clock
    }

    #[must_use]
    pub const fn body(&self) -> &Body {
        &self.body
    }

    /// The agent that authored the event, qualified by the event's machine.
    #[must_use]
    pub fn author(&self) -> Option<AgentId> {
        self.body
            .author()
            .map(|name| AgentId::new(name.clone(), self.origin().clone()))
    }

    /// The deterministic display order shared by every machine.
    #[must_use]
    pub const fn order(&self) -> (u64, &Origin, NonZeroU64) {
        (self.clock, self.origin(), self.id.seq())
    }

    /// The same event as `peer` may store it: content outside its audience becomes an empty placeholder.
    #[must_use]
    pub fn export(&self, peer: &Origin) -> Self {
        match self.body.audience() {
            Some(audience) if !audience.includes(peer) => self.omitted(),
            Some(_) | None => self.clone(),
        }
    }

    /// An empty placeholder that keeps this event's position in its origin's sequence.
    #[must_use]
    pub fn omitted(&self) -> Self {
        Self {
            id: self.id.clone(),
            clock: self.clock,
            body: Body::Omitted {},
        }
    }
}

/// An event with its canonical encoding and content digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    event: Event,
    encoded: String,
    digest: Digest,
}

/// The BLAKE3 digest of an event's canonical encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    #[must_use]
    pub fn of(encoded: &[u8]) -> Self {
        Self(*blake3::hash(encoded).as_bytes())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Sealed {
    pub fn new(event: Event) -> Result<Self, Invalid> {
        let encoded =
            serde_json::to_string(&event).map_err(|_unencodable| Invalid("event encoding"))?;
        if encoded.len() > MAX_EVENT_BYTES {
            return Err(Invalid("event exceeds its size limit"));
        }
        let digest = Digest::of(encoded.as_bytes());
        Ok(Self {
            event,
            encoded,
            digest,
        })
    }

    #[must_use]
    pub const fn event(&self) -> &Event {
        &self.event
    }

    #[must_use]
    pub fn encoded(&self) -> &str {
        &self.encoded
    }

    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    #[must_use]
    pub fn into_event(self) -> Event {
        self.event
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{Body, Chain, Event, Intent, Sealed};
    use crate::chat::fixtures::{agent, ask, card, id, origin};
    use crate::chat::id::Conversation;

    fn decode(text: &str) -> Result<Event, crate::ingress::JsonError> {
        crate::ingress::json(text.as_bytes(), usize::MAX)
    }

    #[test]
    fn decoding_rejects_unknown_fields_invalid_clocks_and_foreign_audiences() {
        let omitted = |extra: &str| {
            alloc::format!(
                r#"{{"origin":"{}","seq":1,"clock":1,"body":{{"kind":"omitted"{extra}}}}}"#,
                origin('a')
            )
        };
        decode(&omitted("")).unwrap();
        decode(&omitted(r#","extra":true"#)).unwrap_err();
        for clock in [0, u64::MAX] {
            Event::new(id('a', 1), clock, Body::Omitted {}).unwrap_err();
        }
        Event::new(id('c', 1), 1, ask(&agent("alice", 'a'), &agent("bob", 'b'))).unwrap_err();
        Event::new(id('a', 1), 1, ask(&agent("alice", 'a'), &agent("bob", 'b'))).unwrap();
    }

    #[test]
    fn asks_cannot_target_their_sender_their_chain_or_another_direct_pair() {
        let valid = ask(&agent("alice", 'a'), &agent("bob", 'b'));
        let Body::Message {
            conversation,
            from,
            text,
            audience,
            ..
        } = valid
        else {
            panic!("fixture is a message");
        };
        let with = |responder, chain, thread: &Conversation| Body::Message {
            conversation: thread.clone(),
            from: from.clone(),
            text: text.clone(),
            audience: audience.clone(),
            intent: Intent::Ask { responder, chain },
            at: 1,
        };
        let bob = agent("bob", 'b');
        let cycle = Chain::try_from(vec![bob.clone()]).unwrap();
        Event::new(id('a', 1), 1, with(bob.clone(), cycle, &conversation)).unwrap_err();
        let other = Conversation::direct(&agent("alice", 'a'), &agent("carol", 'b')).unwrap();
        Event::new(id('a', 1), 1, with(bob, Chain::default(), &other)).unwrap_err();
        let outsider = Body::Message {
            conversation,
            from: agent("mallory", 'a').name().clone(),
            text: text.clone(),
            audience: audience.clone(),
            intent: Intent::Send {},
            at: 1,
        };
        Event::new(id('a', 1), 1, outsider).unwrap_err();
        Chain::try_from(vec![agent("x", 'a'), agent("x", 'a')]).unwrap_err();
        Chain::try_from(
            (0..9)
                .map(|index| agent(&alloc::format!("a{index}"), 'a'))
                .collect::<Vec<_>>(),
        )
        .unwrap_err();
    }

    #[test]
    fn export_hides_content_outside_the_audience_but_keeps_the_sequence() {
        let event =
            Event::new(id('a', 3), 7, ask(&agent("alice", 'a'), &agent("bob", 'b'))).unwrap();
        assert_eq!(event.export(&origin('b')), event);
        let hidden = event.export(&origin('c'));
        assert_eq!((hidden.id(), hidden.clock()), (event.id(), 7));
        assert_eq!(hidden.body(), &Body::Omitted {});
        let profile = Event::new(
            id('a', 4),
            8,
            Body::Profile {
                agent: agent("alice", 'a').name().clone(),
                card: Box::new(card("Alice", "reviewer", &[])),
            },
        )
        .unwrap();
        assert_eq!(profile.export(&origin('c')), profile);
    }

    #[test]
    fn sealing_bounds_the_encoding_and_digests_it_canonically() {
        let event =
            Event::new(id('a', 1), 1, ask(&agent("alice", 'a'), &agent("bob", 'b'))).unwrap();
        let first = Sealed::new(event.clone()).unwrap();
        assert_eq!(first.digest(), Sealed::new(event).unwrap().digest());
        assert_eq!(
            first.digest(),
            super::Digest::of(first.encoded().as_bytes())
        );
    }
}
