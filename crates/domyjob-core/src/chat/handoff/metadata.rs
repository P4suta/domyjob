use serde::{Deserialize, Serialize};

use super::{Project, Scope, Target};
use crate::chat::event::Members;
use crate::chat::id::{AgentId, Audience, Invalid};
use crate::chat::policy;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawMetadata", into = "RawMetadata")]
pub struct Metadata {
    sender: AgentId,
    intended_recipient: Target,
    project: Project,
    scope: Scope,
    permitted_readers: Members,
    audience: Audience,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetadata {
    sender: AgentId,
    intended_recipient: Target,
    project: Project,
    scope: Scope,
    permitted_readers: Members,
}

impl TryFrom<RawMetadata> for Metadata {
    type Error = Invalid;

    fn try_from(raw: RawMetadata) -> Result<Self, Self::Error> {
        Self::new(
            raw.sender,
            raw.intended_recipient,
            (raw.project, raw.scope),
            raw.permitted_readers,
        )
    }
}

impl From<Metadata> for RawMetadata {
    fn from(value: Metadata) -> Self {
        Self {
            sender: value.sender,
            intended_recipient: value.intended_recipient,
            project: value.project,
            scope: value.scope,
            permitted_readers: value.permitted_readers,
        }
    }
}

impl Metadata {
    pub fn new(
        sender: AgentId,
        intended_recipient: Target,
        context: (Project, Scope),
        permitted_readers: Members,
    ) -> Result<Self, Invalid> {
        if let Target::Agent { agent } = &intended_recipient
            && !permitted_readers.contains(agent)
        {
            return Err(Invalid("handoff intended recipient must be a named reader"));
        }
        let audience =
            policy::audience(core::iter::once(&sender).chain(permitted_readers.agents()))?;
        Ok(Self {
            sender,
            intended_recipient,
            project: context.0,
            scope: context.1,
            permitted_readers,
            audience,
        })
    }

    #[must_use]
    pub const fn sender(&self) -> &AgentId {
        &self.sender
    }

    #[must_use]
    pub const fn intended_recipient(&self) -> &Target {
        &self.intended_recipient
    }

    #[must_use]
    pub const fn project(&self) -> &Project {
        &self.project
    }

    #[must_use]
    pub const fn scope(&self) -> &Scope {
        &self.scope
    }

    #[must_use]
    pub const fn permitted_readers(&self) -> &Members {
        &self.permitted_readers
    }

    #[must_use]
    pub const fn audience(&self) -> &Audience {
        &self.audience
    }
}
