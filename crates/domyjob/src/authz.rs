use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::domain::{JobRef, MachineName};
use crate::protocol::{Change, Request, Submission};
use crate::trust::PublicKey;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Capability {
    Submit,
    Observe,
    Fetch,
    Kill,
    Maintain,
}

impl Capability {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.parse() {
            Ok(capability) => Some(capability),
            Err(_unknown) => None,
        }
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
    Submit,
    Kill,
    MaintainedCommand,
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
    use Capability::{Fetch, Kill, Maintain, Observe, Submit};
    use Effect::{Kill as KillEffect, MaintainedCommand, Query, Submit as SubmitEffect};
    match request {
        Request::Hello => nature_of("hello", Always, Query, Unaudited),
        Request::Hold => nature_of("hold", Always, Query, Unaudited),
        Request::Submit { .. } => nature_of("submit", Needs(Submit), SubmitEffect, Audited),
        Request::List { .. } => nature_of("list", Needs(Observe), Query, Unaudited),
        Request::Report => nature_of("report", Needs(Observe), Query, Unaudited),
        Request::Watch => nature_of("watch", Needs(Observe), Query, Unaudited),
        Request::AuditAt { .. } => nature_of("audit-at", Needs(Observe), Query, Unaudited),
        Request::AuditHead => nature_of("audit-head", Needs(Observe), Query, Unaudited),
        Request::Digest { .. } => nature_of("digest", Needs(Observe), Query, Unaudited),
        Request::Search { .. } => nature_of("search", Needs(Observe), Query, Unaudited),
        Request::Status { .. } => nature_of("status", Needs(Observe), Query, Unaudited),
        Request::Wait { .. } => nature_of("wait", Needs(Observe), Query, Unaudited),
        Request::Retry { .. } => nature_of("retry", Needs(Submit), MaintainedCommand, Audited),
        Request::Logs { .. } => nature_of("logs", Needs(Observe), Query, Unaudited),
        Request::Tail { .. } => nature_of("tail", Needs(Observe), Query, Unaudited),
        Request::Kill { .. } => nature_of("kill", Needs(Kill), KillEffect, Audited),
        Request::Clean { .. } => nature_of("clean", Needs(Maintain), MaintainedCommand, Audited),
        Request::Configure { .. } => {
            nature_of("configure", Needs(Maintain), MaintainedCommand, Audited)
        }
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
    nature: Nature,
}

#[derive(Debug)]
pub enum Routed {
    Query(Queried),
    Command(Commanded),
}

#[derive(Debug, Clone, Copy)]
pub enum QueryEffect {}

#[derive(Debug, Clone, Copy)]
pub enum CommandEffect {}

#[derive(Debug)]
pub struct RoutedRequest<E> {
    principal: Principal,
    request: Request,
    nature: Nature,
    effect: std::marker::PhantomData<E>,
}

pub type Queried = RoutedRequest<QueryEffect>;
pub type Commanded = RoutedRequest<CommandEffect>;

#[derive(Debug)]
pub struct AuthorizedSubmission {
    principal: Principal,
    submission: Submission,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigureKind {}
#[derive(Debug, Clone, Copy)]
pub enum CleanKind {}
#[derive(Debug, Clone, Copy)]
pub enum RetryKind {}
#[derive(Debug, Clone, Copy)]
pub enum KillKind {}

#[derive(Debug)]
pub struct AuthorizedCommand<K, P> {
    principal: Principal,
    payload: P,
    kind: std::marker::PhantomData<K>,
}

pub type AuthorizedConfigure = AuthorizedCommand<ConfigureKind, Change>;
pub type AuthorizedClean = AuthorizedCommand<CleanKind, (bool, bool, bool)>;
pub type AuthorizedRetry = AuthorizedCommand<RetryKind, JobRef>;
pub type AuthorizedKill = AuthorizedCommand<KillKind, JobRef>;

#[derive(Debug)]
pub enum CommandAction {
    Configure(AuthorizedConfigure),
    Clean(AuthorizedClean),
    Retry(AuthorizedRetry),
    Kill(AuthorizedKill),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the authorized command is not a submission")]
pub struct NotSubmission;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the authorized command has no non-submission action")]
pub struct NotCommandAction;

mod command_authority {
    pub(super) trait Sealed {}
}

#[expect(
    private_bounds,
    reason = "only authorized commands and submissions may initiate node maintenance"
)]
pub trait CommandAuthority: command_authority::Sealed {}

impl command_authority::Sealed for RoutedRequest<CommandEffect> {}
impl CommandAuthority for RoutedRequest<CommandEffect> {}
impl command_authority::Sealed for AuthorizedSubmission {}
impl CommandAuthority for AuthorizedSubmission {}

impl<K, P> command_authority::Sealed for AuthorizedCommand<K, P> {}
impl<K, P> CommandAuthority for AuthorizedCommand<K, P> {}

impl Authorized {
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    #[must_use]
    pub fn route(self) -> Routed {
        match self.nature.effect {
            Effect::Query => Routed::Query(RoutedRequest {
                principal: self.principal,
                request: self.request,
                nature: self.nature,
                effect: std::marker::PhantomData,
            }),
            Effect::Submit | Effect::Kill | Effect::MaintainedCommand => {
                Routed::Command(RoutedRequest {
                    principal: self.principal,
                    request: self.request,
                    nature: self.nature,
                    effect: std::marker::PhantomData,
                })
            }
        }
    }
}

impl<E> RoutedRequest<E> {
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    #[must_use]
    pub const fn request(&self) -> &Request {
        &self.request
    }
}

impl RoutedRequest<QueryEffect> {
    #[must_use]
    pub fn into_parts(self) -> (Principal, Request) {
        (self.principal, self.request)
    }
}

impl RoutedRequest<CommandEffect> {
    #[must_use]
    pub const fn effect(&self) -> Effect {
        self.nature.effect
    }

    pub fn into_submission(self) -> Result<AuthorizedSubmission, NotSubmission> {
        match self.request {
            Request::Submit { submission } => Ok(AuthorizedSubmission {
                principal: self.principal,
                submission: *submission,
            }),
            Request::Hello
            | Request::Hold
            | Request::List { .. }
            | Request::Status { .. }
            | Request::Wait { .. }
            | Request::Retry { .. }
            | Request::Kill { .. }
            | Request::Logs { .. }
            | Request::Tail { .. }
            | Request::Get { .. }
            | Request::Changes { .. }
            | Request::Report
            | Request::Watch
            | Request::Clean { .. }
            | Request::Configure { .. }
            | Request::AuditAt { .. }
            | Request::AuditHead
            | Request::Digest { .. }
            | Request::Search { .. } => Err(NotSubmission),
        }
    }

    pub fn into_action(self) -> Result<CommandAction, NotCommandAction> {
        let principal = self.principal;
        Ok(match self.request {
            Request::Configure { change } => {
                CommandAction::Configure(AuthorizedCommand::new(principal, change))
            }
            Request::Clean { apply, logs, idle } => {
                CommandAction::Clean(AuthorizedCommand::new(principal, (apply, logs, idle)))
            }
            Request::Retry { job } => CommandAction::Retry(AuthorizedCommand::new(principal, job)),
            Request::Kill { job } => CommandAction::Kill(AuthorizedCommand::new(principal, job)),
            Request::Hello
            | Request::Hold
            | Request::Submit { .. }
            | Request::List { .. }
            | Request::Status { .. }
            | Request::Wait { .. }
            | Request::Logs { .. }
            | Request::Tail { .. }
            | Request::Get { .. }
            | Request::Changes { .. }
            | Request::Report
            | Request::Watch
            | Request::AuditAt { .. }
            | Request::AuditHead
            | Request::Digest { .. }
            | Request::Search { .. } => return Err(NotCommandAction),
        })
    }
}

impl<K, P> AuthorizedCommand<K, P> {
    const fn new(principal: Principal, payload: P) -> Self {
        Self {
            principal,
            payload,
            kind: std::marker::PhantomData,
        }
    }

    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    #[must_use]
    pub const fn payload(&self) -> &P {
        &self.payload
    }
}

impl AuthorizedSubmission {
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    #[must_use]
    pub const fn submission(&self) -> &Submission {
        &self.submission
    }

    #[must_use]
    pub fn into_parts(self) -> (Principal, Submission) {
        (self.principal, self.submission)
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "authorization is the sole boundary that opens an inbound peer request"
)]
pub fn authorize(
    principal: Principal,
    peer: crate::ingress::PeerRequest,
) -> Result<Authorized, Denied> {
    let nature = peer.nature();
    let allowed = match nature.access {
        Access::Always => Ok(()),
        Access::Needs(capability) => match &principal {
            Principal::Owner => Ok(()),
            Principal::Peer { capabilities, .. } if capabilities.contains(&capability) => Ok(()),
            Principal::Peer { .. } => Err(Denied(capability)),
        },
    };
    allowed.map(|()| Authorized {
        principal,
        request: peer.into_request(),
        nature,
    })
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

    fn checked(principal: Principal, request: Request) -> Result<Authorized, Denied> {
        authorize(principal, crate::ingress::PeerRequest::for_test(request))
    }

    fn submission() -> Request {
        Request::Submit {
            submission: Box::new(Submission {
                nonce: "00000000000000000000000000000000".parse().unwrap(),
                name: None,
                command: crate::protocol::Command::Script("true".to_owned()),
                location: crate::protocol::Location::Home,
                env: std::collections::BTreeMap::new(),
                shell: None,
                queue: crate::protocol::Queue::Now,
            }),
        }
    }

    #[test]
    fn command_effects_select_submission_kill_or_maintenance() {
        let job = JobRef::parse_loose("0").unwrap();
        for (request, effect) in [
            (Request::Hello, Effect::Query),
            (submission(), Effect::Submit),
            (Request::Kill { job: job.clone() }, Effect::Kill),
            (Request::Retry { job }, Effect::MaintainedCommand),
            (
                Request::Clean {
                    apply: false,
                    logs: false,
                    idle: false,
                },
                Effect::MaintainedCommand,
            ),
            (
                Request::Configure {
                    change: Change {
                        paused: Some(true),
                        max_jobs: None,
                    },
                },
                Effect::MaintainedCommand,
            ),
        ] {
            assert_eq!(nature(&request).effect, effect);
        }
    }

    #[test]
    fn peers_get_exactly_what_they_were_granted() {
        let kill = || Request::Kill {
            job: JobRef::parse_loose("0").unwrap(),
        };
        checked(Principal::Owner, kill()).unwrap();
        checked(peer(&[]), Request::Hello).unwrap();
        assert_eq!(
            checked(peer(&[Capability::Observe]), kill()).unwrap_err(),
            Denied(Capability::Kill)
        );
        checked(peer(&[Capability::Kill]), kill()).unwrap();
        let pause = || Request::Configure {
            change: Change {
                paused: Some(true),
                max_jobs: None,
            },
        };
        assert_eq!(
            checked(peer(&[Capability::Kill]), pause()).unwrap_err(),
            Denied(Capability::Maintain)
        );
        checked(peer(&[Capability::Maintain]), pause()).unwrap();
        for capability in <Capability as strum::IntoEnumIterator>::iter() {
            assert_eq!(Capability::parse(capability.as_str()), Some(capability));
        }
        assert_eq!(
            checked(peer(&[]), Request::List { limit: 1 }).unwrap_err(),
            Denied(Capability::Observe)
        );
    }

    #[test]
    fn only_a_submitted_command_carries_submission_authority() {
        let submit = submission();
        assert_eq!(
            checked(peer(&[Capability::Observe]), submit.clone()).unwrap_err(),
            Denied(Capability::Submit)
        );
        let approved = checked(peer(&[Capability::Submit]), submit).unwrap();
        let Routed::Command(commanded) = approved.route() else {
            panic!("a submission is a command");
        };
        assert_eq!(commanded.effect(), Effect::Submit);
        let submitted = commanded.into_submission().unwrap();
        assert!(matches!(submitted.principal(), Principal::Peer { .. }));
        assert!(matches!(
            submitted.submission().command,
            crate::protocol::Command::Script(_)
        ));

        let maintenance = checked(
            peer(&[Capability::Maintain]),
            Request::Configure {
                change: Change {
                    paused: Some(true),
                    max_jobs: None,
                },
            },
        )
        .unwrap();
        let Routed::Command(maintenance_command) = maintenance.route() else {
            panic!("configuration is a command");
        };
        assert_eq!(maintenance_command.effect(), Effect::MaintainedCommand);
        assert_eq!(
            maintenance_command.into_submission().unwrap_err(),
            NotSubmission
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

    #[test]
    fn principal_descriptions_include_the_peer_identity() {
        assert_eq!(Principal::Owner.describe(), "owner");
        let key = PublicKey::from_slice(&[9; 32]).unwrap();
        assert_eq!(peer(&[]).describe(), format!("mac ({})", key.fingerprint()));
    }
}
