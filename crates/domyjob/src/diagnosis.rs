use serde::Serialize;

use crate::client::ClientError;
use crate::protocol::RefusalCode;
use crate::remote::RemoteError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Usage,
    Config,
    NotFound,
    Ambiguous,
    Unreachable,
    Forbidden,
    Protocol,
    Security,
    Distribution,
    Local,
    Remote,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diagnosis {
    pub kind: Kind,
    pub hint: Option<&'static str>,
}

const fn plain(kind: Kind) -> Diagnosis {
    Diagnosis { kind, hint: None }
}

const fn hinted(kind: Kind, hint: &'static str) -> Diagnosis {
    Diagnosis {
        kind,
        hint: Some(hint),
    }
}

#[must_use]
pub const fn of_refusal(code: RefusalCode) -> Diagnosis {
    match code {
        RefusalCode::NoSuchJob => hinted(
            Kind::NotFound,
            "`domyjob ls` lists the jobs each machine has",
        ),
        RefusalCode::AmbiguousJob => {
            hinted(Kind::Ambiguous, "give more of the job id, or MACHINE:ID")
        }
        RefusalCode::Forbidden => hinted(
            Kind::Forbidden,
            "the machine's pairing does not allow this; `domyjob trust ls` on it shows what is granted",
        ),
        RefusalCode::BadRequest => plain(Kind::Usage),
        RefusalCode::NoWorkspace => hinted(
            Kind::NotFound,
            "the job ran without sending a directory, so it has no workspace to fetch from",
        ),
        RefusalCode::NoSuchPath => hinted(
            Kind::NotFound,
            "paths are relative to the directory the job ran in; `domyjob run MACHINE -- ls` shows what is there",
        ),
        RefusalCode::NotAFile => hinted(
            Kind::Usage,
            "name one file inside the directory; get copies a single file",
        ),
        RefusalCode::DiskFull => hinted(
            Kind::Remote,
            "the machine's disk is full; domyjob clears its idle workspaces and old job logs there by itself, and if that is not enough, free space on it and try again",
        ),
        RefusalCode::MissingContent | RefusalCode::Storage | RefusalCode::Spawn => {
            plain(Kind::Remote)
        }
    }
}

#[must_use]
pub const fn of_remote(error: &RemoteError) -> Diagnosis {
    match error {
        RemoteError::Config(_) | RemoteError::Template { .. } | RemoteError::Empty { .. } => {
            plain(Kind::Config)
        }
        RemoteError::Start { .. } | RemoteError::Pipe { .. } | RemoteError::Exited { .. } => {
            hinted(
                Kind::Unreachable,
                "check that `ssh MACHINE` works on its own, or that the paired machine is serving",
            )
        }
        RemoteError::Probe { .. } => plain(Kind::Unreachable),
        RemoteError::Silent { .. } => hinted(
            Kind::Unreachable,
            "`ssh MACHINE domyjob node` shows whether it starts at all; an overloaded or wedged machine answers ssh but runs nothing",
        ),
        RemoteError::Unreadable { .. } => plain(Kind::Remote),
        RemoteError::Stream { problem, .. } => match problem {
            crate::framed::Unframed::Truncated | crate::framed::Unframed::Reading(_) => hinted(
                Kind::Unreachable,
                "the connection dropped before everything arrived; run the same command again",
            ),
            crate::framed::Unframed::Malformed(_) | crate::framed::Unframed::Damaged(_) => hinted(
                Kind::Protocol,
                "what arrived does not match what was sent; run it again, and `domyjob doctor` if it repeats",
            ),
            crate::framed::Unframed::Writing(_) => plain(Kind::Local),
            crate::framed::Unframed::Failed(refusal) => of_refusal(refusal.code),
        },
        RemoteError::Garbled { .. }
        | RemoteError::Unexpected { .. }
        | RemoteError::Protocol { .. } => hinted(
            Kind::Protocol,
            "the machine runs a different domyjob; `domyjob setup MACHINE` installs the matching one",
        ),
        RemoteError::Refused { refusal, .. } => of_refusal(refusal.code),
        RemoteError::Dist(_) => plain(Kind::Distribution),
        RemoteError::Newer { .. } => hinted(
            Kind::Protocol,
            "this machine has the older domyjob; `domyjob self update` here, or install the matching build, then try again",
        ),
        RemoteError::Unbuilt { .. } => hinted(
            Kind::Distribution,
            "build this version on that machine and run `domyjob setup MACHINE --from PATH --insecure-unsigned`",
        ),
        RemoteError::Outdated { .. } => hinted(
            Kind::Protocol,
            "build this version for that machine and run `domyjob setup MACHINE --from PATH --insecure-unsigned`, or install a signed release",
        ),
        RemoteError::Tampered { .. }
        | RemoteError::AuditRolledBack { .. }
        | RemoteError::AuditRewritten { .. } => plain(Kind::Security),
        RemoteError::State(_) | RemoteError::Snapshot(_) | RemoteError::Io { .. } => {
            plain(Kind::Local)
        }
    }
}

#[must_use]
pub const fn of_client(error: &ClientError) -> Diagnosis {
    match error {
        ClientError::Remote(remote) => of_remote(remote),
        ClientError::Config(_) | ClientError::Project(_) | ClientError::Runner { .. } => {
            plain(Kind::Config)
        }
        ClientError::Invalid(_) | ClientError::NoInput => plain(Kind::Usage),
        ClientError::Unknown(_) => hinted(
            Kind::NotFound,
            "`domyjob ls` lists jobs; names and latest refer to jobs started from this machine",
        ),
        ClientError::Ambiguous { .. } => hinted(
            Kind::Ambiguous,
            "name one of them as MACHINE:ID, for example the first one listed",
        ),
        ClientError::Snapshot(_)
        | ClientError::Io { .. }
        | ClientError::Index { .. }
        | ClientError::State(_) => plain(Kind::Local),
        ClientError::Panicked => plain(Kind::Internal),
    }
}
