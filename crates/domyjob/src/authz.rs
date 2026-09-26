use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::domain::MachineName;
use crate::protocol::Request;
use crate::trust::PublicKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Submit,
    Observe,
    Fetch,
    Kill,
}

impl Capability {
    pub const ALL: [Self; 4] = [Self::Submit, Self::Observe, Self::Fetch, Self::Kill];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::Observe => "observe",
            Self::Fetch => "fetch",
            Self::Kill => "kill",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == text)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum Submitter {
    Owner,
    Peer { key: PublicKey, label: MachineName },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Owner,
    Peer {
        key: PublicKey,
        label: MachineName,
        capabilities: BTreeSet<Capability>,
    },
}

impl Principal {
    #[must_use]
    pub fn submitter(&self) -> Submitter {
        match self {
            Self::Owner => Submitter::Owner,
            Self::Peer { key, label, .. } => Submitter::Peer {
                key: *key,
                label: label.clone(),
            },
        }
    }

    #[must_use]
    pub fn relation_to(&self, submitter: &Submitter) -> Relation {
        match (self, submitter) {
            (Self::Owner, Submitter::Owner | Submitter::Peer { .. }) => Relation::Oversees,
            (Self::Peer { key, .. }, Submitter::Peer { key: owner, .. }) if key == owner => {
                Relation::Submitted
            }
            (Self::Peer { .. }, Submitter::Peer { .. } | Submitter::Owner) => Relation::Stranger,
        }
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Owner => "owner".to_owned(),
            Self::Peer { key, label, .. } => format!("{label} ({})", key.fingerprint()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Oversees,
    Submitted,
    Stranger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Always,
    Needs(Capability),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Query,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audit {
    Always,
    WhenAPeerAsks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nature {
    pub name: &'static str,
    pub access: Access,
    pub effect: Effect,
    pub audit: Audit,
}

const fn nature_of(name: &'static str, access: Access, effect: Effect, audit: Audit) -> Nature {
    Nature {
        name,
        access,
        effect,
        audit,
    }
}

#[must_use]
pub const fn nature(request: &Request) -> Nature {
    use Access::{Always, Needs};
    use Audit::{Always as Audited, WhenAPeerAsks as Unaudited};
    use Capability::{Fetch, Kill, Observe, Submit};
    use Effect::{Command, Query};
    match request {
        Request::Hello => nature_of("hello", Always, Query, Unaudited),
        Request::Hold => nature_of("hold", Always, Query, Unaudited),
        Request::Missing { .. } => nature_of("missing", Needs(Submit), Query, Unaudited),
        Request::Upload { .. } => nature_of("upload", Needs(Submit), Command, Unaudited),
        Request::Submit { .. } => nature_of("submit", Needs(Submit), Command, Audited),
        Request::List { .. } => nature_of("list", Needs(Observe), Query, Unaudited),
        Request::Report => nature_of("report", Needs(Observe), Query, Unaudited),
        Request::Watch => nature_of("watch", Needs(Observe), Query, Unaudited),
        Request::AuditAt { .. } => nature_of("audit-at", Needs(Observe), Query, Unaudited),
        Request::AuditHead => nature_of("audit-head", Needs(Observe), Query, Unaudited),
        Request::Digest { .. } => nature_of("digest", Needs(Observe), Query, Unaudited),
        Request::Search { .. } => nature_of("search", Needs(Observe), Query, Unaudited),
        Request::Status { .. } => nature_of("status", Needs(Observe), Query, Unaudited),
        Request::Wait { .. } => nature_of("wait", Needs(Observe), Query, Unaudited),
        Request::Logs { .. } => nature_of("logs", Needs(Observe), Query, Unaudited),
        Request::Tail { .. } => nature_of("tail", Needs(Observe), Query, Unaudited),
        Request::Kill { .. } => nature_of("kill", Needs(Kill), Command, Audited),
        Request::Clean { .. } => nature_of("clean", Needs(Kill), Command, Audited),
        Request::Pause { .. } => nature_of("pause", Needs(Kill), Command, Audited),
        Request::Get { .. } => nature_of("get", Needs(Fetch), Query, Audited),
        Request::Changes { .. } => nature_of("changes", Needs(Fetch), Query, Audited),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this requires the {} capability, which the grant does not include", .0.as_str())]
pub struct Denied(pub Capability);

#[derive(Debug)]
pub struct Authorized {
    principal: Principal,
    request: Request,
}

impl Authorized {
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    #[must_use]
    pub fn into_parts(self) -> (Principal, Request) {
        (self.principal, self.request)
    }
}

pub fn authorize(principal: Principal, request: Request) -> Result<Authorized, Denied> {
    let allowed = match (&principal, nature(&request).access) {
        (_, Access::Always) | (Principal::Owner, Access::Needs(_)) => Ok(()),
        (Principal::Peer { capabilities, .. }, Access::Needs(capability)) => {
            if capabilities.contains(&capability) {
                Ok(())
            } else {
                Err(Denied(capability))
            }
        }
    };
    allowed.map(|()| Authorized { principal, request })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::JobRef;

    fn peer(capabilities: &[Capability]) -> Principal {
        Principal::Peer {
            key: PublicKey::from_slice(&[9; 32]).unwrap(),
            label: "mac".parse().unwrap(),
            capabilities: capabilities.iter().copied().collect(),
        }
    }

    #[test]
    fn peers_get_exactly_what_they_were_granted() {
        let kill = || Request::Kill {
            job: JobRef::parse_loose("0").unwrap(),
        };
        authorize(Principal::Owner, kill()).unwrap();
        authorize(peer(&[]), Request::Hello).unwrap();
        assert_eq!(
            authorize(peer(&[Capability::Observe]), kill()).unwrap_err(),
            Denied(Capability::Kill)
        );
        authorize(peer(&[Capability::Kill]), kill()).unwrap();
        assert_eq!(
            authorize(peer(&[]), Request::List { limit: 1 }).unwrap_err(),
            Denied(Capability::Observe)
        );
    }

    #[test]
    fn peers_touch_only_their_own_jobs() {
        let me = peer(&[]);
        let mine = me.submitter();
        let theirs = Submitter::Peer {
            key: PublicKey::from_slice(&[1; 32]).unwrap(),
            label: "other".parse().unwrap(),
        };
        assert_eq!(me.relation_to(&mine), Relation::Submitted);
        assert_eq!(me.relation_to(&theirs), Relation::Stranger);
        assert_eq!(me.relation_to(&Submitter::Owner), Relation::Stranger);
        assert_eq!(Principal::Owner.relation_to(&theirs), Relation::Oversees);
    }
}
