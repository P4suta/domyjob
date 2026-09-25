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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

#[must_use]
pub const fn access(request: &Request) -> Access {
    match request {
        Request::Hello | Request::Hold => Access::Always,
        Request::Missing { .. } | Request::Upload { .. } | Request::Submit { .. } => {
            Access::Needs(Capability::Submit)
        }
        Request::List { .. }
        | Request::Report
        | Request::AuditAt { .. }
        | Request::AuditHead
        | Request::Digest { .. }
        | Request::Search { .. }
        | Request::Status { .. }
        | Request::Wait { .. }
        | Request::Logs { .. }
        | Request::Tail { .. } => Access::Needs(Capability::Observe),
        Request::Kill { .. } | Request::Clean { .. } | Request::Pause { .. } => {
            Access::Needs(Capability::Kill)
        }
        Request::Get { .. } | Request::Changes { .. } => Access::Needs(Capability::Fetch),
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
    let allowed = match (&principal, access(&request)) {
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
