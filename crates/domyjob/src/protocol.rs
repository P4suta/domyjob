use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::authz::Submitter;
use crate::clock::Timestamp;
use crate::domain::{
    BlobId, ChainHash, Concurrency, EnvName, JobId, JobName, JobRef, ProjectKey, RelPath,
};
use crate::terminal::RemoteText;

const FRAMING: &str = "json lines; a stream is u32 big-endian lengths, 0 to end, u32::MAX to beat, then an ending line; changes stream a changed line, the sent manifest, then each file left";

#[must_use]
pub fn wire() -> &'static str {
    static WIRE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    WIRE.get_or_init(|| {
        let described = serde_json::json!({
            "request": schemars::schema_for!(Request),
            "reply": schemars::schema_for!(Reply),
            "frame": schemars::schema_for!(Frame),
            "ending": schemars::schema_for!(crate::framed::Ending),
            "survey": schemars::schema_for!(Survey),
            "changed": schemars::schema_for!(crate::snapshot::Changed),
            "framing": FRAMING,
        });
        let digest = blake3::hash(described.to_string().as_bytes()).to_hex();
        digest.as_str().get(..12).unwrap_or_default().to_owned()
    })
}

#[must_use]
pub fn build_key() -> &'static str {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| format!("{VERSION}-{}", wire()))
}

#[must_use]
pub fn is_newer(theirs: &str) -> bool {
    match (
        semver::Version::parse(theirs),
        semver::Version::parse(VERSION),
    ) {
        (Ok(theirs), Ok(ours)) => theirs > ours,
        (Ok(_) | Err(_), Ok(_) | Err(_)) => false,
    }
}
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "op")]
pub enum Request {
    Hello,
    Hold,
    Missing {
        blobs: Vec<BlobId>,
    },
    Upload {
        count: u64,
    },
    Submit {
        submission: Box<Submission>,
    },
    List {
        limit: u32,
    },
    Status {
        job: JobRef,
    },
    Wait {
        job: JobRef,
    },
    Kill {
        job: JobRef,
    },
    Logs {
        job: JobRef,
        offset: u64,
        follow: Follow,
    },
    Tail {
        job: JobRef,
        lines: u32,
    },
    Get {
        job: JobRef,
        path: RelPath,
    },
    Changes {
        job: JobRef,
    },
    Report,
    Watch,
    Clean {
        apply: bool,
        logs: bool,
        idle: bool,
    },
    Pause {
        paused: bool,
    },
    AuditAt {
        seq: u64,
    },
    AuditHead,
    Digest {
        job: JobRef,
        tail: u32,
    },
    Search {
        job: JobRef,
        pattern: String,
        context: u32,
        limit: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Follow {
    UntilFinished,
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "reply")]
pub enum Reply {
    Hello(Hello),
    Missing {
        blobs: Vec<BlobId>,
    },
    Stored {
        count: u64,
    },
    Job(Box<Job>),
    Jobs {
        jobs: Vec<Job>,
        unreadable: Vec<Unreadable>,
    },
    Stream,
    AuditAt {
        hash: Option<ChainHash>,
    },
    AuditHead(crate::audit::Head),
    Digest(Box<Digest>),
    Found(Found),
    Report(Box<Report>),
    Cleaned(Box<Cleaned>),
    Refused(Refusal),
}

macro_rules! into_variant {
    ($name:ident, $out:ty, $pattern:pat => $value:expr) => {
        pub fn $name(self) -> Result<$out, Box<Self>> {
            match self {
                $pattern => Ok($value),
                other => Err(Box::new(other)),
            }
        }
    };
}

impl Reply {
    into_variant!(into_report, Report, Self::Report(report) => *report);
    into_variant!(into_cleaned, Cleaned, Self::Cleaned(cleaned) => *cleaned);
    into_variant!(into_hello, Hello, Self::Hello(hello) => hello);
    into_variant!(into_missing, Vec<BlobId>, Self::Missing { blobs } => blobs);
    into_variant!(into_stored, u64, Self::Stored { count } => count);
    into_variant!(into_job, Job, Self::Job(job) => *job);
    into_variant!(into_jobs, (Vec<Job>, Vec<Unreadable>), Self::Jobs { jobs, unreadable } => (jobs, unreadable));
    into_variant!(into_stream, (), Self::Stream => ());
    into_variant!(into_audit_at, Option<ChainHash>, Self::AuditAt { hash } => hash);
    into_variant!(into_audit_head, crate::audit::Head, Self::AuditHead(head) => head);
    into_variant!(into_digest, Digest, Self::Digest(digest) => *digest);
    into_variant!(into_found, Found, Self::Found(found) => found);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Digest {
    pub job: Job,
    pub lines: u64,
    pub bytes: u64,
    pub tail: Vec<RemoteText>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FoundLine {
    pub line: u64,
    pub text: RemoteText,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Found {
    pub hits: Vec<FoundLine>,
    pub matched: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub blob: BlobId,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Refusal {
    pub code: RefusalCode,
    pub detail: RemoteText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    BadRequest,
    NoSuchJob,
    AmbiguousJob,
    NoWorkspace,
    MissingContent,
    Forbidden,
    Storage,
    Spawn,
    NoSuchPath,
    NotAFile,
    DiskFull,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Survey {
    pub report: Report,
    pub jobs: Vec<Job>,
}

impl crate::ingress::Ingress for Survey {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cleaned {
    pub applied: bool,
    pub items: Vec<Freeable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Freeable {
    pub what: RemoteText,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub host: RemoteText,
    pub os: RemoteText,
    pub cores: u32,
    pub load_hundredths: Option<[u32; 3]>,
    pub memory_total: u64,
    pub memory_available: u64,
    pub disk_total: u64,
    pub disk_available: u64,
    pub disk_short: bool,
    pub uptime_seconds: u64,
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub wire: RemoteText,
    pub version: RemoteText,
    pub os: RemoteText,
    pub arch: RemoteText,
    pub home: RemoteText,
    pub state: RemoteText,
    pub shell: RemoteText,
    pub binary: RemoteText,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unreadable {
    pub id: JobId,
    pub why: RemoteText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Speaker {
    pub wire: RemoteText,
    pub version: RemoteText,
}

impl Speaker {
    #[must_use]
    pub fn of(hello: &Hello) -> Self {
        Self {
            wire: hello.wire.clone(),
            version: hello.version.clone(),
        }
    }

    #[must_use]
    pub fn glimpse(raw: &[u8]) -> Option<Self> {
        let text = match std::str::from_utf8(raw) {
            Ok(text) => text,
            Err(_not_text) => return None,
        };
        let value = match crate::ingress::foreign_json_envelope(text) {
            Ok(value) => value,
            Err(_not_json) => return None,
        };
        if value.get("reply")?.as_str()? != "hello" {
            return None;
        }
        let wire = value.get("wire")?.as_str()?.to_owned();
        let version = value.get("version")?.as_str()?.to_owned();
        Some(Self {
            wire: RemoteText::new(wire),
            version: RemoteText::new(version),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Command {
    Script(String),
    Argv(Vec<String>),
}

const HEADLINE: usize = 60;

impl Command {
    fn words(&self) -> String {
        let text = match self {
            Self::Script(text) => text.clone(),
            Self::Argv(argv) => argv.join(" "),
        };
        crate::terminal::neutralize(&text.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    #[must_use]
    pub fn display(&self) -> String {
        self.words()
    }

    #[must_use]
    pub fn headline(&self) -> String {
        let words = self.words();
        match words.char_indices().nth(HEADLINE) {
            Some((cut, _)) => format!("{}…", words.get(..cut).unwrap_or(&words)),
            None => words,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Workspace {
    Warm,
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum Revision {
    WorkingDirectory,
    Commit {
        source: String,
        rev: String,
        commit: String,
    },
}

impl Revision {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::WorkingDirectory => "working directory".to_owned(),
            Self::Commit {
                source,
                rev,
                commit,
            } => {
                let short: String = commit.chars().take(12).collect();
                format!("{source} {rev} ({short})")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub project: ProjectKey,
    pub manifest: BlobId,
    pub revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Location {
    Snapshot {
        source: Source,
        subdir: Option<RelPath>,
        workspace: Workspace,
    },
    Home,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub nonce: crate::domain::Nonce,
    pub name: Option<JobName>,
    pub command: Command,
    pub location: Location,
    pub env: BTreeMap<EnvName, String>,
    pub shell: Option<String>,
    pub concurrency: Concurrency,
    pub queue: Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Queue {
    Slot,
    Now,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub id: JobId,
    pub name: Option<JobName>,
    pub command: Command,
    pub location: Location,
    pub env_names: BTreeSet<EnvName>,
    pub shell: Option<String>,
    pub concurrency: Concurrency,
    pub sequence: u64,
    pub submitted_by: Submitter,
    pub submitted_at: Timestamp,
}

impl Spec {
    #[must_use]
    pub const fn source(&self) -> Option<&Source> {
        match &self.location {
            Location::Snapshot { source, .. } => Some(source),
            Location::Home => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "phase")]
pub enum Phase {
    Queued,
    Preparing {
        started_at: Timestamp,
    },
    Running {
        started_at: Timestamp,
        pid: u32,
        workspace: String,
    },
    Finished {
        started_at: Option<Timestamp>,
        finished_at: Timestamp,
        outcome: Outcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "outcome")]
pub enum Outcome {
    Succeeded,
    Failed { exit_code: i32 },
    Killed,
    Errored { reason: RemoteText },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Supervisor {
    Alive,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub spec: Spec,
    pub phase: Phase,
    pub supervisor: Supervisor,
    pub behind: Vec<JobId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Preparing,
    Running,
    Succeeded,
    Failed,
    Killed,
    Errored,
    Lost,
}

impl State {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Errored => "errored",
            Self::Lost => "lost",
        }
    }
}

impl Job {
    #[must_use]
    pub const fn state(&self) -> State {
        match (&self.phase, self.supervisor) {
            (Phase::Finished { outcome, .. }, _) => match outcome {
                Outcome::Succeeded => State::Succeeded,
                Outcome::Failed { .. } => State::Failed,
                Outcome::Killed => State::Killed,
                Outcome::Errored { .. } => State::Errored,
            },
            (_, Supervisor::Gone) => State::Lost,
            (Phase::Queued, Supervisor::Alive) => State::Queued,
            (Phase::Preparing { .. }, Supervisor::Alive) => State::Preparing,
            (Phase::Running { .. }, Supervisor::Alive) => State::Running,
        }
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        matches!(self.phase, Phase::Finished { .. }) || matches!(self.supervisor, Supervisor::Gone)
    }

    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        match &self.phase {
            Phase::Finished {
                outcome: Outcome::Succeeded,
                ..
            } => Some(0),
            Phase::Finished {
                outcome: Outcome::Failed { exit_code },
                ..
            } => Some(*exit_code),
            Phase::Finished { .. }
            | Phase::Queued
            | Phase::Preparing { .. }
            | Phase::Running { .. } => None,
        }
    }

    #[must_use]
    pub fn took(&self) -> Option<crate::clock::Elapsed> {
        match &self.phase {
            Phase::Finished {
                started_at: Some(start),
                finished_at,
                ..
            } => Some(start.until(*finished_at)),
            Phase::Running { started_at, .. } | Phase::Preparing { started_at } => {
                Some(started_at.until(Timestamp::observe()))
            }
            Phase::Finished {
                started_at: None, ..
            }
            | Phase::Queued => None,
        }
    }

    #[must_use]
    pub const fn reason(&self) -> Option<&RemoteText> {
        match &self.phase {
            Phase::Finished {
                outcome: Outcome::Errored { reason },
                ..
            } => Some(reason),
            Phase::Finished { .. }
            | Phase::Queued
            | Phase::Preparing { .. }
            | Phase::Running { .. } => None,
        }
    }

    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(
            self.phase,
            Phase::Finished {
                outcome: Outcome::Succeeded,
                ..
            }
        )
    }
}

impl crate::ingress::Ingress for Request {}
impl crate::ingress::Ingress for Reply {}
impl crate::ingress::Ingress for Frame {}
impl crate::ingress::Ingress for Phase {}
impl crate::ingress::Ingress for Spec {}
impl crate::ingress::Ingress for Job {}
impl crate::ingress::Ingress for Hello {}
impl crate::ingress::Ingress for Digest {}
impl crate::ingress::Ingress for Found {}
impl crate::ingress::Ingress for Submission {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_exact() {
        let hello: Request = serde_json::from_str(r#"{"op":"hello"}"#).unwrap();
        assert_eq!(hello, Request::Hello);
        let kill: Request = serde_json::from_str(r#"{"op":"kill","job":"01ABC"}"#).unwrap();
        assert!(matches!(kill, Request::Kill { .. }));
        serde_json::from_str::<Request>(r#"{"op":"kill","job":"01ABC","x":1}"#).unwrap_err();
        serde_json::from_str::<Request>(r#"{"op":"kill","job":"not-an-id"}"#).unwrap_err();
        serde_json::from_str::<Request>(r#"{"op":"reboot"}"#).unwrap_err();
    }

    #[test]
    fn commands_read_as_one_safe_line() {
        let script = Command::Script("cargo build\n  && cargo test \u{1b}[2J".into());
        assert_eq!(script.display(), "cargo build && cargo test \u{FFFD}[2J");
        let long = Command::Argv(vec!["x".repeat(100)]);
        assert_eq!(long.headline().chars().count(), 61);
        assert!(long.headline().ends_with('…'));
        assert_eq!(Command::Argv(vec!["ls".into()]).headline(), "ls");
    }

    #[test]
    fn any_hello_shape_still_tells_its_wire_and_version() {
        let newer = br#"{"reply":"hello","wire":"0123456789ab","version":"9.0.0","novel":{"x":1}}"#;
        assert_eq!(
            Speaker::glimpse(newer),
            Some(Speaker {
                wire: RemoteText::new("0123456789ab".into()),
                version: RemoteText::new("9.0.0".into())
            })
        );
        assert_eq!(
            Speaker::glimpse(br#"{"reply":"job","wire":"w","version":"x"}"#),
            None
        );
        assert_eq!(Speaker::glimpse(b"not json"), None);
        assert_eq!(
            Speaker::glimpse(br#"{"reply":"hello","wire":9,"version":"x"}"#),
            None
        );
    }

    #[test]
    fn the_wire_is_a_fixed_digest_of_every_message_shape() {
        assert_eq!(wire().len(), 12);
        assert!(wire().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(wire(), wire());
        let request = serde_json::to_string(&schemars::schema_for!(Request)).unwrap();
        assert!(request.contains("changes") && request.contains("report"));
    }

    #[test]
    fn only_a_release_above_this_one_counts_as_newer() {
        assert!(is_newer("999.0.0"));
        assert!(!is_newer(VERSION));
        assert!(!is_newer("0.0.0-alpha"));
        assert!(!is_newer("not a version"));
    }

    #[test]
    fn the_hello_reply_keeps_its_shape_across_versions() {
        let hello = Hello {
            wire: RemoteText::new("w".into()),
            version: RemoteText::new("v".into()),
            os: RemoteText::new("o".into()),
            arch: RemoteText::new("a".into()),
            home: RemoteText::new("h".into()),
            state: RemoteText::new("s".into()),
            shell: RemoteText::new("sh".into()),
            binary: RemoteText::new("b".into()),
        };
        let value = serde_json::to_value(Reply::Hello(hello)).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "arch", "binary", "home", "os", "reply", "shell", "state", "version", "wire"
            ]
        );
    }

    #[test]
    fn phases_round_trip() {
        let phase = Phase::Finished {
            started_at: Some(Timestamp::at_millis(1)),
            finished_at: Timestamp::at_millis(2),
            outcome: Outcome::Failed { exit_code: 3 },
        };
        let text = serde_json::to_string(&phase).unwrap();
        assert_eq!(serde_json::from_str::<Phase>(&text).unwrap(), phase);
        assert!(text.contains(r#""outcome":"failed""#));
    }

    proptest::proptest! {
        #[test]
        fn headlines_are_one_short_safe_line(text in ".*") {
            let headline = Command::Script(text).headline();
            proptest::prop_assert!(headline.chars().count() <= 61);
            proptest::prop_assert!(!headline.contains('\n') && !headline.contains('\x1b'));
        }
    }
}
