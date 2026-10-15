use alloc::format;
use alloc::string::String;
use serde::{Deserialize, Serialize};

use super::id::{AgentId, Audience, Conversation, EventId, Invalid, Line, MAX_TEXT, RoomId, Text};

mod decision;
mod metadata;
pub use decision::{
    DeliveryOutcome, DescriptorMatch, ObservationFacts, ReadFacts, ReaderGrant,
    observation_after_delivery, observation_decision, read_decision,
};
pub use metadata::Metadata;

pub const MAX_CONTEXT_BYTES: usize = 256;
pub const MAX_BODY_BYTES: usize = MAX_TEXT;

macro_rules! context {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Line<MAX_CONTEXT_BYTES>);

        impl TryFrom<String> for $name {
            type Error = Invalid;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Line::try_from(value).map(Self)
            }
        }

        impl $name {
            #[must_use]
            pub const fn line(&self) -> &Line<MAX_CONTEXT_BYTES> {
                &self.0
            }
        }
    };
}
context!(Project);
context!(Scope);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TeamId(RoomId);

impl TryFrom<String> for TeamId {
    type Error = Invalid;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match Conversation::try_from(value)? {
            Conversation::Room(room) => Ok(Self(room)),
            Conversation::Direct(_) => Err(Invalid("handoff team")),
        }
    }
}

impl From<TeamId> for String {
    fn from(value: TeamId) -> Self {
        format!("{}", value.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Agent { agent: AgentId },
    Team { team: TeamId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct ReadLimit(usize);

impl ReadLimit {
    pub const MAX: Self = Self(MAX_BODY_BYTES);
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for ReadLimit {
    type Error = Invalid;
    fn try_from(value: usize) -> Result<Self, Self::Error> {
        if value == 0 || value > MAX_BODY_BYTES {
            Err(Invalid("handoff read limit"))
        } else {
            Ok(Self(value))
        }
    }
}

impl From<ReadLimit> for usize {
    fn from(value: ReadLimit) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadRefusal {
    #[error("the handoff belongs to another project")]
    Project,
    #[error("the handoff belongs to another scope")]
    Scope,
    #[error("the handoff metadata does not match the selected descriptor")]
    Descriptor,
    #[error("the acting agent is not an explicitly permitted handoff reader")]
    Reader,
    #[error("the handoff read limit is invalid")]
    Limit,
    #[error("the handoff body does not fit the admitted read")]
    Body,
    #[error("the handoff observation audience differs from its parent")]
    Audience,
    #[error("the handoff observation must follow its parent")]
    Order,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawDescriptor", into = "RawDescriptor")]
pub struct Descriptor {
    id: EventId,
    metadata: Metadata,
    body_bytes: ReadLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDescriptor {
    id: EventId,
    metadata: Metadata,
    body_bytes: ReadLimit,
}

impl TryFrom<RawDescriptor> for Descriptor {
    type Error = Invalid;
    fn try_from(raw: RawDescriptor) -> Result<Self, Self::Error> {
        if raw.id.origin() != raw.metadata.sender().origin() {
            return Err(Invalid("handoff sender origin"));
        }
        Ok(Self {
            id: raw.id,
            metadata: raw.metadata,
            body_bytes: raw.body_bytes,
        })
    }
}

impl From<Descriptor> for RawDescriptor {
    fn from(value: Descriptor) -> Self {
        Self {
            id: value.id,
            metadata: value.metadata,
            body_bytes: value.body_bytes,
        }
    }
}

impl Descriptor {
    pub fn new(id: EventId, metadata: Metadata, text: &Text) -> Result<Self, Invalid> {
        RawDescriptor {
            id,
            metadata,
            body_bytes: ReadLimit::try_from(text.as_str().len())?,
        }
        .try_into()
    }

    #[must_use]
    pub const fn id(&self) -> &EventId {
        &self.id
    }

    #[must_use]
    pub const fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    #[must_use]
    pub const fn body_bytes(&self) -> usize {
        self.body_bytes.bytes()
    }

    pub fn authorize<'a>(
        &'a self,
        reader: &'a AgentId,
        selected: &Self,
        limit: ReadLimit,
    ) -> Result<Permit<'a>, ReadRefusal> {
        let facts = ReadFacts {
            project_matches: self.metadata.project() == selected.metadata.project(),
            scope_matches: self.metadata.scope() == selected.metadata.scope(),
            descriptor: if self == selected {
                DescriptorMatch::Exact
            } else {
                DescriptorMatch::Foreign
            },
            reader: reader_grant(self, reader),
        };
        read_decision(facts, self.body_bytes(), limit.bytes())?;
        Ok(Permit {
            descriptor: self,
            reader,
        })
    }

    pub fn admit_observation(
        &self,
        observation: &Observation,
        issuer: (&AgentId, &Audience),
        clocks: (u64, u64),
    ) -> Result<(), ReadRefusal> {
        observation_decision(ObservationFacts {
            parent_matches: *self.id() == observation.handoff,
            issuer_matches: *issuer.0 == observation.reader,
            audience_matches: *issuer.1 == *self.metadata.audience(),
            reader: reader_grant(self, &observation.reader),
            parent_clock: clocks.0,
            observation_clock: clocks.1,
        })
    }
}

fn reader_grant(descriptor: &Descriptor, reader: &AgentId) -> ReaderGrant {
    if descriptor.metadata.permitted_readers().contains(reader) {
        ReaderGrant::Named
    } else {
        ReaderGrant::Absent
    }
}

#[derive(Debug)]
pub struct Permit<'a> {
    descriptor: &'a Descriptor,
    reader: &'a AgentId,
}

impl Permit<'_> {
    #[must_use]
    pub const fn reader(&self) -> &AgentId {
        self.reader
    }

    #[must_use]
    pub const fn descriptor(&self) -> &Descriptor {
        self.descriptor
    }

    pub fn check_body(
        &self,
        id: &EventId,
        metadata: &Metadata,
        text: &Text,
    ) -> Result<(), ReadRefusal> {
        if self.descriptor.id != *id
            || self.descriptor.metadata != *metadata
            || self.descriptor.body_bytes() != text.as_str().len()
        {
            return Err(ReadRefusal::Descriptor);
        }
        Ok(())
    }

    #[must_use]
    pub fn observation(&self) -> Observation {
        Observation {
            handoff: self.descriptor.id.clone(),
            reader: self.reader.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    handoff: EventId,
    reader: AgentId,
}

impl Observation {
    #[must_use]
    pub const fn handoff(&self) -> &EventId {
        &self.handoff
    }

    #[must_use]
    pub const fn reader(&self) -> &AgentId {
        &self.reader
    }
}
