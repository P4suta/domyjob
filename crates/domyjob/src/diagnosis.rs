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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub kind: Kind,
    pub hint: Option<String>,
}

const SLOT: &str = "{machine}";

impl Diagnosis {
    #[must_use]
    pub fn about(self, machine: Option<&str>) -> Self {
        Self {
            kind: self.kind,
            hint: self
                .hint
                .map(|hint| hint.replace(SLOT, machine.unwrap_or("<machine>"))),
        }
    }
}

const fn plain(kind: Kind) -> Diagnosis {
    Diagnosis { kind, hint: None }
}

fn hinted(kind: Kind, hint: &'static str) -> Diagnosis {
    Diagnosis {
        kind,
        hint: Some(hint.to_owned()),
    }
}

#[must_use]
pub fn of_refusal(code: RefusalCode) -> Diagnosis {
    match code {
        RefusalCode::NoSuchJob => hinted(
            Kind::NotFound,
            "`domyjob ls` lists the jobs each machine has",
        ),
        RefusalCode::AmbiguousJob => {
            hinted(Kind::Ambiguous, "give more of the job id, or {machine}:ID")
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
            "paths are relative to the directory the job ran in; `domyjob run {machine} -- ls` shows what is there",
        ),
        RefusalCode::NotAFile => hinted(
            Kind::Usage,
            "name one file inside the directory; get copies a single file",
        ),
        RefusalCode::Paused => hinted(
            Kind::Remote,
            "the machine is paused for maintenance; `domyjob machines resume {machine}` lets it take jobs again",
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
pub fn of_remote(error: &RemoteError) -> Diagnosis {
    remote_template(error).about(error.machine())
}

fn remote_template(error: &RemoteError) -> Diagnosis {
    match error {
        RemoteError::Config(_) | RemoteError::Template { .. } | RemoteError::Empty { .. } => {
            plain(Kind::Config)
        }
        RemoteError::Start { .. } | RemoteError::Pipe { .. } | RemoteError::Exited { .. } => {
            hinted(
                Kind::Unreachable,
                "check that `ssh {machine}` works on its own, or that the paired machine is serving",
            )
        }
        RemoteError::Probe { .. } => plain(Kind::Unreachable),
        RemoteError::Silent { .. } => hinted(
            Kind::Unreachable,
            "an overloaded or wedged machine answers ssh but runs nothing; `domyjob doctor {machine}` checks it again",
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
            "the machine runs a different domyjob; `domyjob setup {machine}` installs the matching one",
        ),
        RemoteError::Refused { refusal, .. } => of_refusal(refusal.code),
        RemoteError::Dist(_) | RemoteError::BuildMismatch { .. } => plain(Kind::Distribution),
        RemoteError::Newer { .. } => hinted(
            Kind::Protocol,
            "this machine has the older domyjob; `domyjob self update` here, or install the matching build, then try again",
        ),
        RemoteError::Unbuilt { .. } => hinted(
            Kind::Distribution,
            "from domyjob's own source checkout, `domyjob setup {machine} --build` builds and installs the matching one there",
        ),
        RemoteError::Outdated { .. } => hinted(
            Kind::Protocol,
            "from domyjob's own source checkout, `domyjob setup {machine} --build` builds and installs the matching one there, or install a signed release",
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
pub fn of_client(error: &ClientError) -> Diagnosis {
    match error {
        ClientError::Remote(remote) => of_remote(remote),
        ClientError::Config(crate::config::ConfigError::Unknown { .. }) => hinted(
            Kind::Usage,
            "add it with `domyjob machines add NAME`, or reach an ssh host directly as ssh:HOST",
        ),
        ClientError::Config(crate::config::ConfigError::ThisMachine(_)) => hinted(
            Kind::Usage,
            "`domyjob self uninstall` removes domyjob from this machine",
        ),
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
            "name one of them as {machine}:ID, for example the first one listed",
        ),
        ClientError::Snapshot(_)
        | ClientError::Io { .. }
        | ClientError::Index { .. }
        | ClientError::State(_) => plain(Kind::Local),
        ClientError::Panicked => plain(Kind::Internal),
        ClientError::Unpacked { .. } => hinted(
            Kind::Protocol,
            "what arrived does not match what was sent; pull again, and `domyjob doctor` if it repeats",
        ),
        ClientError::Elsewhere { .. } => hinted(
            Kind::NotFound,
            "names and `latest` mean jobs sent from this project; run it from there, or use the job's id",
        ),
        ClientError::NotSent { .. } => hinted(
            Kind::Usage,
            "pull brings back jobs `domyjob run` sent with a directory from this machine; `domyjob get JOB PATH` copies one file",
        ),
    }
}

#[must_use]
pub fn of_pull(error: &crate::pull::PullError) -> Diagnosis {
    use crate::pull::PullError;
    let (kind, hint) = match error {
        PullError::Diverged(_) => (
            Kind::Usage,
            Some("commit or set aside your own edits to those files, then pull again"),
        ),
        PullError::Edited(_) => (
            Kind::Usage,
            Some("set aside your edits to those files, then undo again"),
        ),
        PullError::Gone(_) => (
            Kind::Usage,
            Some("put the directory back where the job was sent from"),
        ),
        PullError::NeverPulled(_) => (Kind::Usage, None),
        PullError::Malformed(_) => (
            Kind::Protocol,
            Some(
                "what arrived does not match what was sent; pull again, and `domyjob doctor` if it repeats",
            ),
        ),
        PullError::NotKept(_)
        | PullError::Io { .. }
        | PullError::State(_)
        | PullError::Lock(_)
        | PullError::Tree(_) => (Kind::Local, None),
    };
    Diagnosis {
        kind,
        hint: hint.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_names_the_machine_it_is_about_and_leaves_no_placeholder() {
        let unbuilt = RemoteError::Unbuilt {
            machine: "linux".to_owned(),
            was: String::new(),
        };
        let hint = of_remote(&unbuilt).hint.unwrap();
        assert!(hint.contains("domyjob setup linux --build"), "{hint}");
        let unknown = of_refusal(RefusalCode::Paused).about(None).hint.unwrap();
        assert!(unknown.contains("<machine>"), "{unknown}");
        let placeholder = "machine".to_uppercase();
        assert!(!include_str!("diagnosis.rs").contains(&placeholder));
    }
}
