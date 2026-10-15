use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use super::card::{Card, MachineCard};
use super::handoff::{Metadata as HandoffMetadata, Observation as HandoffObservation};
use super::id::{
    AgentId, AgentName, Audience, Conversation, EventId, Invalid, Line, Origin, RoomName, Text,
};
use super::notification::{Activity, Delivery, MILLIS_PER_SECOND, Notice};

pub const MAX_EVENT_BYTES: usize = 400 * 1024;
pub const MAX_CHAIN: usize = 8;
pub const MAX_MEMBERS: usize = 64;
const DUPLICATE_WINDOW: usize = 2;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3600;
const SECONDS_PER_DAY: u64 = 86_400;

macro_rules! agent_list {
    ($(#[$doc:meta])* $name:ident, $min:literal, $max:expr, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
                if sorted.windows(DUPLICATE_WINDOW).any(|pair| matches!(pair, [first, second] if first == second)) {
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
    #[derive(Default)]
    Chain,
    0,
    MAX_CHAIN,
    "delegation chain"
);
agent_list!(Members, 1, MAX_MEMBERS, "room members");

impl Chain {
    pub fn extended(&self, waiting: AgentId) -> Result<Self, Invalid> {
        let mut agents = self.0.clone();
        agents.push(waiting);
        Self::try_from(agents)
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Stamp(u64);

impl Stamp {
    #[must_use]
    pub const fn millis(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn from_unix_millis(millis: u64) -> Self {
        Self(millis)
    }

    #[must_use]
    pub const fn age(self, now: Self) -> Age {
        Age(now.0.saturating_sub(self.0) / MILLIS_PER_SECOND)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Age(u64);

impl core::fmt::Display for Age {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let seconds = self.0;
        match seconds {
            0..SECONDS_PER_MINUTE => write!(formatter, "{seconds}s ago"),
            SECONDS_PER_MINUTE..SECONDS_PER_HOUR => {
                write!(formatter, "{}m ago", seconds / SECONDS_PER_MINUTE)
            }
            SECONDS_PER_HOUR..SECONDS_PER_DAY => {
                write!(formatter, "{}h ago", seconds / SECONDS_PER_HOUR)
            }
            _ => write!(formatter, "{}d ago", seconds / SECONDS_PER_DAY),
        }
    }
}

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
        at: Stamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<Notice>,
    },
    Handoff {
        metadata: HandoffMetadata,
        text: Text,
        at: Stamp,
    },
    HandoffRead {
        observation: HandoffObservation,
        audience: Audience,
    },
    Receipt {
        request: EventId,
        conversation: Conversation,
        audience: Audience,
        agent: AgentName,
        delivery: Delivery,
    },
    Activity {
        agent: AgentName,
        activity: Activity,
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
            }
            | Self::Receipt {
                conversation,
                audience,
                ..
            } => Some((conversation, audience)),
            Self::Profile { .. }
            | Self::Left { .. }
            | Self::Machine { .. }
            | Self::Room { .. }
            | Self::RoomClosed { .. }
            | Self::Activity { .. }
            | Self::Handoff { .. }
            | Self::HandoffRead { .. }
            | Self::Omitted {} => None,
        }
    }

    #[must_use]
    pub fn audience(&self) -> Option<&Audience> {
        match self {
            Self::Handoff { metadata, .. } => Some(metadata.audience()),
            Self::HandoffRead { audience, .. } => Some(audience),
            Self::Profile { .. }
            | Self::Left { .. }
            | Self::Machine { .. }
            | Self::Room { .. }
            | Self::RoomClosed { .. }
            | Self::Message { .. }
            | Self::Receipt { .. }
            | Self::Activity { .. }
            | Self::TurnStarted { .. }
            | Self::Resolved { .. }
            | Self::Omitted {} => self.thread().map(|(_, audience)| audience),
        }
    }

    #[must_use]
    pub fn conversation(&self) -> Option<&Conversation> {
        self.thread().map(|(conversation, _)| conversation)
    }

    #[must_use]
    pub const fn request(&self) -> Option<&EventId> {
        if let Self::HandoffRead { observation, .. } = self {
            return Some(observation.handoff());
        }
        if let Self::Message {
            intent: Intent::Reply { request },
            ..
        }
        | Self::TurnStarted { request, .. }
        | Self::Resolved { request, .. }
        | Self::Receipt { request, .. } = self
        {
            Some(request)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn author(&self) -> Option<&AgentName> {
        match self {
            Self::Message { from: agent, .. }
            | Self::Profile { agent, .. }
            | Self::Left { agent }
            | Self::TurnStarted { agent, .. }
            | Self::Resolved { agent, .. }
            | Self::Receipt { agent, .. }
            | Self::Activity { agent, .. } => Some(agent),
            Self::Handoff { metadata, .. } => Some(metadata.sender().name()),
            Self::HandoffRead { observation, .. } => Some(observation.reader().name()),
            Self::Machine { .. }
            | Self::Room { .. }
            | Self::RoomClosed { .. }
            | Self::Omitted {} => None,
        }
    }
}

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

fn valid_thread(conversation: &Conversation, audience: &Audience, author: &AgentId) -> bool {
    match conversation {
        Conversation::Direct(pair) => {
            let [first, second] = pair.agents();
            let (first, second) = (first.origin(), second.origin());
            pair.includes(author)
                && match first.cmp(second) {
                    core::cmp::Ordering::Equal => {
                        matches!(audience.members(), [only] if only == first)
                    }
                    core::cmp::Ordering::Less => {
                        matches!(audience.members(), [lower, upper] if lower == first && upper == second)
                    }
                    core::cmp::Ordering::Greater => {
                        matches!(audience.members(), [lower, upper] if lower == second && upper == first)
                    }
                }
        }
        Conversation::Room(_) => true,
    }
}

fn valid_notice(
    sender: &AgentId,
    (conversation, audience): (&Conversation, &Audience),
    intent: &Intent,
    notice: &Notice,
) -> bool {
    !(notice.recipients.contains(sender)
        || notice.recipients.agents().iter().any(|agent| {
            !audience.includes(agent.origin())
                || matches!(conversation, Conversation::Direct(pair) if !pair.includes(agent))
        }) || notice.question.as_ref().is_some_and(|question| {
        question.fallback() == sender
            || !matches!(intent, Intent::Ask { chain, .. } if !chain.contains(question.fallback()))
    })
        || matches!(intent, Intent::Ask { responder, .. } if !notice.recipients.contains(responder)))
}

impl Event {
    pub fn new(id: EventId, clock: u64, body: Body) -> Result<Self, Invalid> {
        if clock == 0 || clock == u64::MAX {
            return Err(Invalid("event clock"));
        }
        if let Body::Handoff { metadata, .. } = &body
            && metadata.sender().origin() != id.origin()
        {
            return Err(Invalid("handoff sender origin"));
        }
        if let Body::HandoffRead {
            observation,
            audience,
        } = &body
            && (observation.reader().origin() != id.origin() || !audience.includes(id.origin()))
        {
            return Err(Invalid("handoff observation author or audience"));
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
        if let Body::Message {
            conversation,
            from,
            audience,
            notice: Some(notice),
            intent,
            ..
        } = &body
        {
            let sender = AgentId::new(from.clone(), id.origin().clone());
            if !valid_notice(&sender, (conversation, audience), intent, notice) {
                return Err(Invalid("message notification recipients"));
            }
        }
        if matches!(
            body,
            Body::Receipt {
                delivery: Delivery::Pending,
                ..
            }
        ) {
            return Err(Invalid(
                "a receipt must record delivery, reading, or acknowledgment",
            ));
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

    #[must_use]
    pub fn author(&self) -> Option<AgentId> {
        self.body
            .author()
            .map(|name| AgentId::new(name.clone(), self.origin().clone()))
    }

    #[must_use]
    pub const fn order(&self) -> (u64, &Origin, NonZeroU64) {
        (self.clock, self.origin(), self.id.seq())
    }

    #[must_use]
    pub fn export(&self, peer: &Origin) -> Self {
        match self.body.audience() {
            Some(audience) if !audience.includes(peer) => self.omitted(),
            Some(_) | None => self.clone(),
        }
    }

    #[must_use]
    pub fn omitted(&self) -> Self {
        Self {
            id: self.id.clone(),
            clock: self.clock,
            body: Body::Omitted {},
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    event: Event,
    encoded: String,
    digest: Digest,
}

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

    use super::{Body, Chain, Event, Intent, Sealed, Stamp};
    use crate::chat::fixtures::{agent, ask, card, id, origin};
    use crate::chat::id::Conversation;

    fn decode(text: &str) -> Result<Event, crate::ingress::JsonError> {
        crate::ingress::json(text.as_bytes(), usize::MAX)
    }

    #[test]
    fn message_age_displays_whole_units_at_each_boundary() {
        let sent = Stamp::from_unix_millis(0);
        for (millis, expected) in [
            (0, "0s ago"),
            (999, "0s ago"),
            (1000, "1s ago"),
            (59_999, "59s ago"),
            (60_000, "1m ago"),
            (3_599_999, "59m ago"),
            (3_600_000, "1h ago"),
            (86_399_999, "23h ago"),
            (86_400_000, "1d ago"),
            (172_800_000, "2d ago"),
        ] {
            assert_eq!(
                alloc::format!("{}", sent.age(Stamp::from_unix_millis(millis))),
                expected,
                "age at {millis} milliseconds"
            );
        }
        assert_eq!(
            alloc::format!(
                "{}",
                Stamp::from_unix_millis(1000).age(Stamp::from_unix_millis(0))
            ),
            "0s ago"
        );
    }

    #[test]
    fn outcome_names_match_their_wire_names() {
        for (outcome, name) in [
            (super::Outcome::Failed, "failed"),
            (super::Outcome::Interrupted, "interrupted"),
            (super::Outcome::Unavailable, "unavailable"),
            (super::Outcome::Withdrawn, "withdrawn"),
        ] {
            assert_eq!(outcome.as_str(), name);
            assert_eq!(
                serde_json::to_string(&outcome).unwrap(),
                alloc::format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn replies_and_turn_endings_keep_their_request_identity() {
        let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
        let request = id('a', 1);
        let mut reply = ask(&bob, &alice);
        assert_eq!(reply.request(), None);
        if let Body::Message { intent, .. } = &mut reply {
            *intent = Intent::Reply {
                request: request.clone(),
            };
        }
        let conversation = Conversation::direct(&alice, &bob).unwrap();
        let audience = crate::chat::policy::audience([&alice, &bob]).unwrap();
        for body in [
            reply,
            Body::TurnStarted {
                request: request.clone(),
                conversation: conversation.clone(),
                audience: audience.clone(),
                agent: bob.name().clone(),
            },
            Body::Resolved {
                request: request.clone(),
                conversation,
                audience,
                agent: bob.name().clone(),
                outcome: super::Outcome::Failed,
            },
        ] {
            assert_eq!(body.request(), Some(&request));
        }
        assert_eq!(Body::Omitted {}.request(), None);
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
            at: Stamp::from_unix_millis(1),
            notice: None,
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
            at: Stamp::from_unix_millis(1),
            notice: None,
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
    fn direct_threads_require_exact_origins_and_full_agent_identity() {
        for (first_origin, second_origin) in [('a', 'b'), ('b', 'a'), ('a', 'a')] {
            let first = agent("alice", first_origin);
            let second = agent("bob", second_origin);
            let conversation = Conversation::direct(&first, &second).unwrap();
            assert_eq!(conversation, Conversation::direct(&second, &first).unwrap());
            let exact = crate::chat::policy::audience([&first, &second]).unwrap();
            let outsiders = [
                agent("mallory", first_origin),
                agent("mallory", second_origin),
            ];
            for author in [&first, &second] {
                assert!(super::valid_thread(&conversation, &exact, author));
            }
            for author in &outsiders {
                assert!(!super::valid_thread(&conversation, &exact, author));
            }
            let extra =
                crate::chat::policy::audience([&first, &second, &agent("carol", 'c')]).unwrap();
            let wrong = crate::chat::policy::audience([&agent("carol", 'c')]).unwrap();
            for audience in [&extra, &wrong] {
                assert!(!super::valid_thread(&conversation, audience, &first));
            }
            if first_origin != second_origin {
                let missing = crate::chat::policy::audience([&first]).unwrap();
                assert!(!super::valid_thread(&conversation, &missing, &first));
                let substituted =
                    crate::chat::policy::audience([&first, &agent("carol", 'c')]).unwrap();
                assert!(!super::valid_thread(&conversation, &substituted, &first));
            }
        }
    }

    #[test]
    fn room_thread_validation_leaves_author_and_audience_admission_to_the_caller() {
        let owner = agent("alice", 'a');
        let conversation = Conversation::Room(crate::chat::id::RoomId::new(
            owner.origin().clone(),
            crate::chat::id::RoomName::try_from(alloc::string::String::from("work")).unwrap(),
        ));
        let audience = crate::chat::policy::audience([&owner]).unwrap();
        assert!(super::valid_thread(
            &conversation,
            &audience,
            &agent("outsider", 'c')
        ));
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

    fn maximal_notification_event() -> Event {
        use crate::chat::id::{
            AgentId, AgentName, Audience, EventId, Line, MAX_AUDIENCE, MAX_TEXT, Origin, RoomId,
            RoomName, Text,
        };
        use crate::chat::notification::{
            MAX_EXPECTED_BYTES, MAX_FALLBACK_SECONDS, MAX_SUBJECT_BYTES, Notice, Question,
        };
        use core::num::NonZeroU64;

        const MAX_HANDLE_BYTES: usize = 64;
        const ORIGIN_ENTROPY_BYTES: usize = 16;
        let origins: Vec<_> = (0..MAX_AUDIENCE)
            .map(|index| Origin::from_entropy([u8::try_from(index).unwrap(); ORIGIN_ENTROPY_BYTES]))
            .collect();
        let local = origins.first().unwrap().clone();
        let recipients: Vec<_> = origins
            .iter()
            .enumerate()
            .map(|(index, origin)| {
                AgentId::new(
                    AgentName::try_from(alloc::format!("{index:0MAX_HANDLE_BYTES$}")).unwrap(),
                    origin.clone(),
                )
            })
            .collect();
        let responder = recipients.first().unwrap().clone();
        let chain = Chain::try_from(
            (0..super::MAX_CHAIN)
                .map(|index| {
                    AgentId::new(
                        AgentName::try_from(alloc::format!(
                            "c{index:0width$}",
                            width = MAX_HANDLE_BYTES - 1
                        ))
                        .unwrap(),
                        local.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let fallback = AgentId::new(
            AgentName::try_from("f".repeat(MAX_HANDLE_BYTES)).unwrap(),
            local.clone(),
        );
        Event::new(
            EventId::new(local.clone(), NonZeroU64::new(u64::MAX).unwrap()),
            u64::MAX - 1,
            Body::Message {
                conversation: Conversation::Room(RoomId::new(
                    local,
                    RoomName::try_from("r".repeat(MAX_HANDLE_BYTES)).unwrap(),
                )),
                from: AgentName::try_from("s".repeat(MAX_HANDLE_BYTES)).unwrap(),
                text: Text::try_from("\u{0001}".repeat(MAX_TEXT)).unwrap(),
                audience: Audience::try_from(origins).unwrap(),
                intent: Intent::Ask { responder, chain },
                at: Stamp::from_unix_millis(u64::MAX),
                notice: Some(Notice {
                    recipients: super::Members::try_from(recipients).unwrap(),
                    urgent: false,
                    subject: Some(Line::try_from("\"".repeat(MAX_SUBJECT_BYTES)).unwrap()),
                    question: Some(
                        Question::new(
                            Line::try_from("\"".repeat(MAX_EXPECTED_BYTES)).unwrap(),
                            MAX_FALLBACK_SECONDS,
                            fallback,
                        )
                        .unwrap(),
                    ),
                }),
            },
        )
        .unwrap()
    }

    #[test]
    fn maximal_notification_fields_still_seal_within_the_event_limit() {
        const MAXIMAL_NOTIFICATION_BYTES: usize = 404_147;
        let sealed = Sealed::new(maximal_notification_event()).unwrap();
        assert_eq!(sealed.encoded().len(), MAXIMAL_NOTIFICATION_BYTES);
        assert!(sealed.encoded().len() < super::MAX_EVENT_BYTES);
        assert_eq!(decode(sealed.encoded()).unwrap(), *sealed.event());
    }
}
