use alloc::borrow::ToOwned;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroU64;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid chat {0}")]
pub struct Invalid(pub &'static str);

fn lower_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

const fn check(valid: bool, what: &'static str) -> Result<(), Invalid> {
    if valid { Ok(()) } else { Err(Invalid(what)) }
}

validated_string!(Origin, Invalid, |text| check(
    lower_hex(text, 32),
    "machine identity"
));

impl Origin {
    #[must_use]
    pub fn from_entropy(bytes: [u8; 16]) -> Self {
        Self(format!("{:032x}", u128::from_be_bytes(bytes)))
    }
}

fn valid_handle(text: &str) -> bool {
    let lower = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    (1..=64).contains(&text.len())
        && text.bytes().next().is_some_and(lower)
        && text
            .bytes()
            .all(|byte| lower(byte) || matches!(byte, b'.' | b'-' | b'_'))
}

validated_string!(AgentName, Invalid, |text| check(
    valid_handle(text),
    "agent name"
));
validated_string!(RoomName, Invalid, |text| check(
    valid_handle(text),
    "room name"
));

fn split_pair<A, B>(text: &str, separator: char, what: &'static str) -> Result<(A, B), Invalid>
where
    A: TryFrom<String, Error = Invalid>,
    B: TryFrom<String, Error = Invalid>,
{
    let (first, second) = text.rsplit_once(separator).ok_or(Invalid(what))?;
    Ok((
        A::try_from(first.to_owned())?,
        B::try_from(second.to_owned())?,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId {
    name: AgentName,
    origin: Origin,
}

impl AgentId {
    #[must_use]
    pub const fn new(name: AgentName, origin: Origin) -> Self {
        Self { name, origin }
    }

    #[must_use]
    pub const fn name(&self) -> &AgentName {
        &self.name
    }

    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }
}

impl TryFrom<String> for AgentId {
    type Error = Invalid;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let (name, origin) = split_pair(&text, '@', "agent address")?;
        Ok(Self { name, origin })
    }
}

impl From<AgentId> for String {
    fn from(value: AgentId) -> Self {
        format!("{value}")
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.name, self.origin)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventId {
    origin: Origin,
    seq: NonZeroU64,
}

impl EventId {
    #[must_use]
    pub const fn new(origin: Origin, seq: NonZeroU64) -> Self {
        Self { origin, seq }
    }

    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    #[must_use]
    pub const fn seq(&self) -> NonZeroU64 {
        self.seq
    }
}

impl TryFrom<String> for EventId {
    type Error = Invalid;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let (origin, sequence) = text.rsplit_once(':').ok_or(Invalid("event ID"))?;
        if !lower_hex(sequence, 16) {
            return Err(Invalid("event ID"));
        }
        let seq = match u64::from_str_radix(sequence, 16) {
            Ok(value) => NonZeroU64::new(value),
            Err(_malformed) => None,
        }
        .ok_or(Invalid("event ID"))?;
        Ok(Self {
            origin: Origin::try_from(origin.to_owned())?,
            seq,
        })
    }
}

impl From<EventId> for String {
    fn from(value: EventId) -> Self {
        format!("{value}")
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{:016x}", self.origin, self.seq)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoomId {
    origin: Origin,
    name: RoomName,
}

impl RoomId {
    #[must_use]
    pub const fn new(origin: Origin, name: RoomName) -> Self {
        Self { origin, name }
    }

    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    #[must_use]
    pub const fn name(&self) -> &RoomName {
        &self.name
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "room:{}:{}", self.origin, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Conversation {
    Direct(DirectPair),
    Room(RoomId),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DirectPair {
    first: AgentId,
    second: AgentId,
}

impl DirectPair {
    pub fn between(one: &AgentId, other: &AgentId) -> Result<Self, Invalid> {
        let (first, second) = match one.cmp(other) {
            core::cmp::Ordering::Less => (one, other),
            core::cmp::Ordering::Greater => (other, one),
            core::cmp::Ordering::Equal => return Err(Invalid("direct conversation")),
        };
        Ok(Self {
            first: first.clone(),
            second: second.clone(),
        })
    }

    #[must_use]
    pub fn includes(&self, agent: &AgentId) -> bool {
        self.first == *agent || self.second == *agent
    }

    #[must_use]
    pub fn other(&self, agent: &AgentId) -> Option<&AgentId> {
        if self.first == *agent {
            Some(&self.second)
        } else if self.second == *agent {
            Some(&self.first)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn agents(&self) -> [&AgentId; 2] {
        [&self.first, &self.second]
    }
}

impl Conversation {
    pub fn direct(one: &AgentId, other: &AgentId) -> Result<Self, Invalid> {
        DirectPair::between(one, other).map(Self::Direct)
    }

    #[must_use]
    pub fn admits_author(&self, agent: &AgentId) -> bool {
        match self {
            Self::Direct(pair) => pair.includes(agent),
            Self::Room(_) => true,
        }
    }
}

impl TryFrom<String> for Conversation {
    type Error = Invalid;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if let Some(pair) = text.strip_prefix("dm:") {
            let (first, second): (AgentId, AgentId) = split_pair(pair, ',', "conversation")?;
            let direct = DirectPair::between(&first, &second)?;
            return if direct.first == first {
                Ok(Self::Direct(direct))
            } else {
                Err(Invalid("conversation"))
            };
        }
        let room = text.strip_prefix("room:").ok_or(Invalid("conversation"))?;
        let (origin, name) = split_pair(room, ':', "conversation")?;
        Ok(Self::Room(RoomId { origin, name }))
    }
}

impl From<Conversation> for String {
    fn from(value: Conversation) -> Self {
        format!("{value}")
    }
}

impl fmt::Display for Conversation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct(pair) => write!(formatter, "dm:{},{}", pair.first, pair.second),
            Self::Room(room) => write!(formatter, "{room}"),
        }
    }
}

macro_rules! bounded_text {
    ($(#[$doc:meta])* $name:ident, $multiline:literal, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name<const MAX: usize>(String);

        impl<const MAX: usize> TryFrom<String> for $name<MAX> {
            type Error = Invalid;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                if valid_text(&text, MAX, $multiline) {
                    Ok(Self(text))
                } else {
                    Err(Invalid($what))
                }
            }
        }

        impl<const MAX: usize> From<$name<MAX>> for String {
            fn from(value: $name<MAX>) -> Self {
                value.0
            }
        }

        impl<const MAX: usize> $name<MAX> {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

fn valid_text(text: &str, max: usize, multiline: bool) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.trim() == text
        && !text
            .chars()
            .any(|character| character.is_control() && !(multiline && character == '\n'))
}

bounded_text!(Line, false, "single-line text");
bounded_text!(Paragraph, true, "paragraph");

pub const MAX_TEXT: usize = 64 * 1024;

validated_string!(Text, Invalid, |text| check(
    text.len() <= MAX_TEXT && !text.trim().is_empty() && !text.contains('\0'),
    "message text"
));

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<Origin>", into = "Vec<Origin>")]
pub struct Audience(Vec<Origin>);

pub const MAX_AUDIENCE: usize = 64;

impl TryFrom<Vec<Origin>> for Audience {
    type Error = Invalid;

    fn try_from(members: Vec<Origin>) -> Result<Self, Self::Error> {
        if members.is_empty()
            || members.len() > MAX_AUDIENCE
            || members
                .windows(2)
                .any(|pair| matches!(pair, [first, second] if first >= second))
        {
            return Err(Invalid("audience"));
        }
        Ok(Self(members))
    }
}

impl TryFrom<BTreeSet<Origin>> for Audience {
    type Error = Invalid;

    fn try_from(members: BTreeSet<Origin>) -> Result<Self, Self::Error> {
        Self::try_from(members.into_iter().collect::<Vec<_>>())
    }
}

impl From<Audience> for Vec<Origin> {
    fn from(value: Audience) -> Self {
        value.0
    }
}

impl Audience {
    #[must_use]
    pub fn includes(&self, origin: &Origin) -> bool {
        self.0.binary_search(origin).is_ok()
    }

    #[must_use]
    pub fn members(&self) -> &[Origin] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::string::String;
    use alloc::vec;

    use super::{
        AgentId, AgentName, Audience, Conversation, DirectPair, EventId, Line, Origin, Paragraph,
        Text,
    };
    use crate::chat::fixtures::{agent, origin};

    #[test]
    fn malformed_addresses_and_event_ids_are_refused_part_by_part() {
        let machine = origin('a');
        AgentId::try_from(alloc::format!("reviewer{machine}")).unwrap_err();
        AgentId::try_from(alloc::format!("Bad@{machine}")).unwrap_err();
        AgentId::try_from("reviewer@nothex".to_owned()).unwrap_err();
        EventId::try_from(alloc::format!("zz:{:016x}", 1)).unwrap_err();
        EventId::try_from(alloc::format!("{machine}:{:016x}", 0)).unwrap_err();
        EventId::try_from(alloc::format!("{machine}:{:016x}", 1)).unwrap();
    }

    #[test]
    fn text_holds_its_length_limit_exactly() {
        Line::<8>::try_from("x".repeat(8)).unwrap();
        Line::<8>::try_from("x".repeat(9)).unwrap_err();
    }

    #[test]
    fn chat_handles_include_their_64_byte_boundary() {
        let longest = "a".repeat(64);
        assert_eq!(
            AgentName::try_from(longest.clone()).unwrap().as_str(),
            longest
        );
        assert_eq!(
            super::RoomName::try_from(longest.clone()).unwrap().as_str(),
            longest
        );
        super::RoomName::try_from("a".repeat(65)).unwrap_err();
    }

    #[test]
    fn only_the_pair_authors_in_a_direct_conversation() {
        let (alice, bob, carol) = (agent("alice", 'a'), agent("bob", 'b'), agent("carol", 'c'));
        let direct = Conversation::direct(&alice, &bob).unwrap();
        assert!(direct.admits_author(&alice) && direct.admits_author(&bob));
        assert!(!direct.admits_author(&carol));
        let room = super::RoomId::new(
            origin('a'),
            super::RoomName::try_from("all".to_owned()).unwrap(),
        );
        assert!(Conversation::Room(room).admits_author(&carol));
    }

    #[test]
    fn identities_round_trip_through_their_canonical_text() {
        let id = agent("reviewer", 'a');
        assert_eq!(AgentId::try_from(String::from(id.clone())).unwrap(), id);
        let event = EventId::try_from(alloc::format!("{}:{:016x}", origin('b'), 42)).unwrap();
        assert_eq!(event.seq().get(), 42);
        assert_eq!(
            EventId::try_from(String::from(event.clone())).unwrap(),
            event
        );
        let direct = Conversation::direct(&id, &agent("builder", 'b')).unwrap();
        assert_eq!(
            Conversation::try_from(String::from(direct.clone())).unwrap(),
            direct
        );
        let room = Conversation::try_from(alloc::format!("room:{}:release", origin('c'))).unwrap();
        assert_eq!(
            Conversation::try_from(String::from(room.clone())).unwrap(),
            room
        );
    }

    #[test]
    fn malformed_identities_cannot_be_constructed() {
        for invalid in ["", "a", &"A".repeat(32), &"g".repeat(32), &"a".repeat(33)] {
            Origin::try_from(invalid.to_owned()).unwrap_err();
        }
        for invalid in ["", "Reviewer", "-x", "a b", "a@b", "a:b", &"a".repeat(65)] {
            AgentName::try_from(invalid.to_owned()).unwrap_err();
        }
        for invalid in [
            alloc::format!("{}:{:016x}", origin('a'), 0),
            alloc::format!("{}:{:016X}", origin('a'), 10),
            alloc::format!("{}:{:x}", origin('a'), 10),
            "reviewer@a".to_owned(),
        ] {
            EventId::try_from(invalid).unwrap_err();
        }
        let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
        for invalid in [
            "dm:abc".to_owned(),
            "room:x:y".to_owned(),
            "room:".to_owned(),
            "other".to_owned(),
            alloc::format!("dm:{bob},{alice}"),
            alloc::format!("dm:{alice},{alice}"),
        ] {
            Conversation::try_from(invalid).unwrap_err();
        }
    }

    #[test]
    fn direct_conversations_are_symmetric_and_name_both_agents() {
        let (first, second) = (agent("a", '1'), agent("b", '2'));
        let pair = DirectPair::between(&first, &second).unwrap();
        assert_eq!(pair, DirectPair::between(&second, &first).unwrap());
        assert_eq!(pair.other(&first), Some(&second));
        assert_eq!(pair.other(&agent("c", '2')), None);
        DirectPair::between(&first, &first).unwrap_err();
    }

    #[test]
    fn text_values_are_bounded_and_free_of_controls() {
        Text::try_from(" \n".to_owned()).unwrap_err();
        Text::try_from("a\0".to_owned()).unwrap_err();
        Text::try_from("x".repeat(super::MAX_TEXT.saturating_add(1))).unwrap_err();
        Text::try_from("結果\n".to_owned()).unwrap();
        Line::<8>::try_from("two\nlines".to_owned()).unwrap_err();
        Line::<8>::try_from(" padded".to_owned()).unwrap_err();
        Line::<8>::try_from("123456789".to_owned()).unwrap_err();
        Paragraph::<16>::try_from("two\nlines".to_owned()).unwrap();
        Paragraph::<16>::try_from("tab\there".to_owned()).unwrap_err();
    }

    #[test]
    fn audiences_are_sorted_unique_and_bounded() {
        Audience::try_from(vec![origin('b'), origin('a')]).unwrap_err();
        Audience::try_from(vec![origin('a'), origin('a')]).unwrap_err();
        Audience::try_from(vec![]).unwrap_err();
        let audience = Audience::try_from(vec![origin('a'), origin('b')]).unwrap();
        assert!(audience.includes(&origin('b')));
        assert!(!audience.includes(&origin('c')));
        assert_eq!(vec::Vec::from(audience), vec![origin('a'), origin('b')]);

        let members: vec::Vec<_> = (0..=64_u8)
            .map(|last| {
                let mut entropy = [0; 16];
                *entropy.last_mut().unwrap() = last;
                Origin::from_entropy(entropy)
            })
            .collect();
        Audience::try_from(members.clone()).unwrap_err();
        let boundary: vec::Vec<_> = members.into_iter().take(super::MAX_AUDIENCE).collect();
        let largest = Audience::try_from(boundary).unwrap();
        assert_eq!(largest.members().len(), super::MAX_AUDIENCE);
    }
}
