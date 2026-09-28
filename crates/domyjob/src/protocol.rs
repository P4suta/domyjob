use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::authz::Submitter;
use crate::clock::Timestamp;
use crate::domain::{
    BlobId, ChainHash, Concurrency, EnvName, JobId, JobName, JobRef, ProjectKey, RelPath,
};
use crate::terminal::RemoteText;

const FRAMING: &str = "json lines; a snapshot submission sends its manifest frame, receives need_blobs, sends exactly those blob frames, then receives the job; a stream is u32 big-endian lengths, 0 to end, u32::MAX to beat, then an ending line; changes stream a changed line, the sent manifest, then each file left";

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
    KEY.get_or_init(|| format!("{VERSION}-{}-{BUILD_STAMP}", wire()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionRelation {
    Newer,
    OlderOrEqual,
    Unparsable,
}

#[must_use]
pub fn version_relation(theirs: &str) -> VersionRelation {
    match (
        semver::Version::parse(theirs),
        semver::Version::parse(VERSION),
    ) {
        (Ok(theirs), Ok(ours)) if theirs > ours => VersionRelation::Newer,
        (Ok(_), Ok(_)) => VersionRelation::OlderOrEqual,
        (Err(_), _) | (Ok(_), Err(_)) => VersionRelation::Unparsable,
    }
}
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD_STAMP: &str = env!("DOMYJOB_BUILD_STAMP");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "op")]
pub enum Request {
    Hello,
    Hold,
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
    Retry {
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
    Configure {
        change: Change,
    },
    AuditAt {
        epoch: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "reply")]
pub enum Reply {
    Hello(Hello),
    NeedBlobs {
        blobs: Vec<BlobId>,
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
    #[must_use]
    pub(crate) const fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            Self::Hello(_)
            | Self::NeedBlobs { .. }
            | Self::Job(_)
            | Self::Jobs { .. }
            | Self::Stream
            | Self::AuditAt { .. }
            | Self::AuditHead(_)
            | Self::Digest(_)
            | Self::Found(_)
            | Self::Report(_)
            | Self::Cleaned(_) => None,
        }
    }

    #[must_use]
    pub(crate) const fn needs_content_retry(&self) -> bool {
        match self.refusal() {
            Some(refusal) => match refusal.code {
                RefusalCode::MissingContent => true,
                RefusalCode::BadRequest
                | RefusalCode::NoSuchJob
                | RefusalCode::AmbiguousJob
                | RefusalCode::NoWorkspace
                | RefusalCode::Forbidden
                | RefusalCode::Storage
                | RefusalCode::Spawn
                | RefusalCode::NoSuchPath
                | RefusalCode::NotAFile
                | RefusalCode::DiskFull
                | RefusalCode::Paused => false,
            },
            None => false,
        }
    }

    into_variant!(into_report, Report, Self::Report(report) => *report);
    into_variant!(into_cleaned, Cleaned, Self::Cleaned(cleaned) => *cleaned);
    into_variant!(into_hello, Hello, Self::Hello(hello) => hello);
    into_variant!(into_need_blobs, Vec<BlobId>, Self::NeedBlobs { blobs } => blobs);
    into_variant!(into_job, Job, Self::Job(job) => *job);
    into_variant!(into_jobs, (Vec<Job>, Vec<Unreadable>), Self::Jobs { jobs, unreadable } => (jobs, unreadable));
    into_variant!(into_stream, (), Self::Stream => ());
    into_variant!(into_audit_at, Option<ChainHash>, Self::AuditAt { hash } => hash);
    into_variant!(into_audit_head, crate::audit::Head, Self::AuditHead(head) => head);
    into_variant!(into_digest, Digest, Self::Digest(digest) => *digest);
    into_variant!(into_found, Found, Self::Found(found) => found);
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    pub items: CleanReportItems,
}

pub(crate) const CLEAN_DETAIL_LIMIT: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct CleanReportItems(
    #[serde(deserialize_with = "bounded_clean_items")]
    #[schemars(length(max = CLEAN_DETAIL_LIMIT + 1))]
    Vec<Freeable>,
);

impl CleanReportItems {
    pub(crate) fn from_parts(
        details: [Option<Freeable>; CLEAN_DETAIL_LIMIT],
        summary: Option<Freeable>,
    ) -> Self {
        let mut items: Vec<_> = details.into_iter().flatten().collect();
        items.extend(summary);
        Self(items)
    }

    #[cfg(test)]
    pub(crate) fn one(item: Freeable) -> Self {
        let mut details = std::array::from_fn(|_| None);
        details[0] = Some(item);
        Self::from_parts(details, None)
    }
}

impl std::ops::Deref for CleanReportItems {
    type Target = [Freeable];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn bounded_clean_items<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Freeable>, D::Error> {
    struct Bounded;

    impl<'de> serde::de::Visitor<'de> for Bounded {
        type Value = Vec<Freeable>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "at most {} cleanable items",
                CLEAN_DETAIL_LIMIT + 1
            )
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut items = Vec::with_capacity(CLEAN_DETAIL_LIMIT + 1);
            while items.len() < CLEAN_DETAIL_LIMIT + 1 {
                match sequence.next_element()? {
                    Some(item) => items.push(item),
                    None => return Ok(items),
                }
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(<A::Error as serde::de::Error>::custom(
                    "too many cleanable items",
                ));
            }
            Ok(items)
        }
    }

    deserializer.deserialize_seq(Bounded)
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
    pub disk: DiskSpace,
    pub uptime_seconds: u64,
    pub paused: bool,
    pub max_jobs: Concurrency,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "state")]
pub enum DiskSpace {
    Measured {
        total: u64,
        available: u64,
        short: bool,
    },
    Unavailable {
        reason: RemoteText,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub paused: bool,
    pub max_jobs: Concurrency,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            paused: false,
            max_jobs: Concurrency::DEFAULT,
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub paused: Option<bool>,
    pub max_jobs: Option<Concurrency>,
}

impl Settings {
    #[must_use]
    pub fn with(self, change: Change) -> Self {
        Self {
            paused: change.paused.unwrap_or(self.paused),
            max_jobs: change.max_jobs.unwrap_or(self.max_jobs),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub wire: RemoteText,
    pub version: RemoteText,
    pub build: RemoteText,
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

impl Location {
    #[must_use]
    pub const fn source(&self) -> Option<&Source> {
        match self {
            Self::Snapshot { source, .. } => Some(source),
            Self::Home => None,
        }
    }
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
    pub queue: Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Queue {
    Slot,
    Now,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
        self.location.source()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "phase")]
pub enum Phase {
    Queued,
    Preparing {
        started_at: Timestamp,
    },
    Starting {
        started_at: Timestamp,
        workspace: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhaseKind {
    Queued,
    Preparing,
    Starting,
    Running,
    Finished,
}

impl Phase {
    #[must_use]
    pub(crate) const fn kind(&self) -> PhaseKind {
        match self {
            Self::Queued => PhaseKind::Queued,
            Self::Preparing { .. } => PhaseKind::Preparing,
            Self::Starting { .. } => PhaseKind::Starting,
            Self::Running { .. } => PhaseKind::Running,
            Self::Finished { .. } => PhaseKind::Finished,
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub spec: Spec,
    pub phase: Phase,
    pub supervisor: Supervisor,
    pub behind: Vec<JobId>,
    pub notes: Vec<RemoteText>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Preparing,
    Running,
    RestartPending,
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
            Self::RestartPending => "restart_pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Errored => "errored",
            Self::Lost => "lost",
        }
    }

    #[must_use]
    pub const fn counts_as_running(self) -> bool {
        match self {
            Self::Preparing | Self::Running => true,
            Self::Queued
            | Self::RestartPending
            | Self::Succeeded
            | Self::Failed
            | Self::Killed
            | Self::Errored
            | Self::Lost => false,
        }
    }

    #[must_use]
    pub const fn has_timing_sample(self) -> bool {
        match self {
            Self::Succeeded | Self::Failed => true,
            Self::Queued
            | Self::Preparing
            | Self::Running
            | Self::RestartPending
            | Self::Killed
            | Self::Errored
            | Self::Lost => false,
        }
    }
}

impl Job {
    const fn outcome(&self) -> Option<&Outcome> {
        match &self.phase {
            Phase::Finished { outcome, .. } => Some(outcome),
            Phase::Queued
            | Phase::Preparing { .. }
            | Phase::Starting { .. }
            | Phase::Running { .. } => None,
        }
    }

    #[must_use]
    pub const fn state(&self) -> State {
        match (&self.phase, self.supervisor) {
            (Phase::Finished { outcome, .. }, Supervisor::Alive | Supervisor::Gone) => {
                match outcome {
                    Outcome::Succeeded => State::Succeeded,
                    Outcome::Failed { .. } => State::Failed,
                    Outcome::Killed => State::Killed,
                    Outcome::Errored { .. } => State::Errored,
                }
            }
            (Phase::Queued | Phase::Preparing { .. }, Supervisor::Gone) => State::RestartPending,
            (Phase::Starting { .. } | Phase::Running { .. }, Supervisor::Gone) => State::Lost,
            (Phase::Queued, Supervisor::Alive) => State::Queued,
            (Phase::Preparing { .. }, Supervisor::Alive) => State::Preparing,
            (Phase::Starting { .. } | Phase::Running { .. }, Supervisor::Alive) => State::Running,
        }
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        match (&self.phase, self.supervisor) {
            (Phase::Finished { .. }, Supervisor::Alive | Supervisor::Gone)
            | (Phase::Starting { .. } | Phase::Running { .. }, Supervisor::Gone) => true,
            (Phase::Queued | Phase::Preparing { .. }, Supervisor::Alive | Supervisor::Gone)
            | (Phase::Starting { .. } | Phase::Running { .. }, Supervisor::Alive) => false,
        }
    }

    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        match self.outcome() {
            Some(Outcome::Succeeded) => Some(0),
            Some(Outcome::Failed { exit_code }) => Some(*exit_code),
            Some(Outcome::Killed | Outcome::Errored { .. }) | None => None,
        }
    }

    #[must_use]
    pub const fn took_at(&self, now: Timestamp) -> Option<crate::clock::Elapsed> {
        match &self.phase {
            Phase::Finished {
                started_at: Some(start),
                finished_at,
                ..
            } => Some(start.until(*finished_at)),
            Phase::Running { started_at, .. }
            | Phase::Starting { started_at, .. }
            | Phase::Preparing { started_at } => Some(started_at.until(now)),
            Phase::Finished {
                started_at: None, ..
            }
            | Phase::Queued => None,
        }
    }

    #[must_use]
    pub const fn reason(&self) -> Option<&RemoteText> {
        match self.outcome() {
            Some(Outcome::Errored { reason }) => Some(reason),
            Some(Outcome::Succeeded | Outcome::Failed { .. } | Outcome::Killed) | None => None,
        }
    }

    #[must_use]
    pub const fn succeeded(&self) -> bool {
        match self.outcome() {
            Some(Outcome::Succeeded) => true,
            Some(Outcome::Failed { .. } | Outcome::Killed | Outcome::Errored { .. }) | None => {
                false
            }
        }
    }
}

impl crate::ingress::Ingress for Reply {}
impl crate::ingress::Ingress for Frame {}
impl crate::ingress::Ingress for Phase {}
impl crate::ingress::Ingress for Spec {}
impl crate::ingress::Ingress for Settings {}
impl crate::ingress::Ingress for Job {}
impl crate::ingress::Ingress for Hello {}
impl crate::ingress::Ingress for Digest {}
impl crate::ingress::Ingress for Found {}
impl crate::ingress::Ingress for CleanReportItems {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_exact() {
        let hello: crate::ingress::PeerRequest =
            crate::ingress::json_text(r#"{"op":"hello"}"#).unwrap();
        assert_eq!(hello.audit().0.name, "hello");
        let kill: crate::ingress::PeerRequest =
            crate::ingress::json_text(r#"{"op":"kill","job":"01ABC"}"#).unwrap();
        assert_eq!(kill.audit().0.name, "kill");
        crate::ingress::json_text::<crate::ingress::PeerRequest>(
            r#"{"op":"kill","job":"01ABC","x":1}"#,
        )
        .unwrap_err();
        crate::ingress::json_text::<crate::ingress::PeerRequest>(
            r#"{"op":"kill","job":"not-an-id"}"#,
        )
        .unwrap_err();
        crate::ingress::json_text::<crate::ingress::PeerRequest>(r#"{"op":"reboot"}"#).unwrap_err();
    }

    #[test]
    fn request_fuzz_seeds_cross_the_peer_boundary() {
        for (name, bytes) in [
            (
                "hello",
                &include_bytes!("../../../fuzz/seeds/requests/hello.json")[..],
            ),
            (
                "list",
                &include_bytes!("../../../fuzz/seeds/requests/list.json")[..],
            ),
            (
                "search",
                &include_bytes!("../../../fuzz/seeds/requests/search.json")[..],
            ),
            (
                "clean",
                &include_bytes!("../../../fuzz/seeds/requests/clean.json")[..],
            ),
            (
                "configure",
                &include_bytes!("../../../fuzz/seeds/requests/configure.json")[..],
            ),
            (
                "submit",
                &include_bytes!("../../../fuzz/seeds/requests/submit.json")[..],
            ),
        ] {
            let peer: crate::ingress::PeerRequest = crate::ingress::json(bytes).unwrap();
            assert_eq!(peer.audit().0.name, name);
            crate::authz::authorize(crate::authz::Principal::Owner, peer).unwrap();
        }
    }

    #[test]
    fn clean_reports_refuse_more_than_the_bounded_detail_and_summary_slots() {
        let one = Freeable {
            what: RemoteText::new("workspace".to_owned()),
            bytes: 1,
        };
        let within = serde_json::to_string(&vec![one.clone(); CLEAN_DETAIL_LIMIT + 1]).unwrap();
        let parsed: CleanReportItems = crate::ingress::json_text(&within).unwrap();
        assert_eq!(parsed.len(), CLEAN_DETAIL_LIMIT + 1);
        let excess = serde_json::to_string(&vec![one; CLEAN_DETAIL_LIMIT + 2]).unwrap();
        crate::ingress::json_text::<CleanReportItems>(&excess).unwrap_err();
        let schema = serde_json::to_value(schemars::schema_for!(CleanReportItems)).unwrap();
        assert_eq!(
            schema.get("maxItems").and_then(serde_json::Value::as_u64),
            Some(crate::domain::len_u64(CLEAN_DETAIL_LIMIT + 1))
        );
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
    fn managed_binary_paths_identify_the_exact_source_build() {
        assert!(build_key().ends_with(BUILD_STAMP));
    }

    #[test]
    fn version_relation_distinguishes_newer_older_and_invalid_versions() {
        assert_eq!(version_relation("999.0.0"), VersionRelation::Newer);
        assert_eq!(version_relation(VERSION), VersionRelation::OlderOrEqual);
        assert_eq!(
            version_relation("0.0.0-alpha"),
            VersionRelation::OlderOrEqual
        );
        assert_eq!(
            version_relation("not a version"),
            VersionRelation::Unparsable
        );
    }

    #[test]
    fn the_hello_reply_keeps_its_shape_across_versions() {
        let hello = Hello {
            wire: RemoteText::new("w".into()),
            version: RemoteText::new("v".into()),
            build: RemoteText::new("b".into()),
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
                "arch", "binary", "build", "home", "os", "reply", "shell", "state", "version",
                "wire"
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
        assert_eq!(crate::ingress::json_text::<Phase>(&text).unwrap(), phase);
        assert!(text.contains(r#""outcome":"failed""#));
    }

    #[test]
    fn active_duration_uses_the_supplied_snapshot() {
        let mut job = crate::view::tests::sample();
        let started_at = Timestamp::at_millis(1_000);
        job.phase = Phase::Preparing { started_at };
        assert_eq!(
            job.took_at(Timestamp::at_millis(4_000))
                .unwrap()
                .to_string(),
            "3s"
        );
        assert_eq!(
            job.took_at(Timestamp::at_millis(6_000))
                .unwrap()
                .to_string(),
            "5s"
        );
        job.phase = Phase::Finished {
            started_at: Some(started_at),
            finished_at: Timestamp::at_millis(4_000),
            outcome: Outcome::Succeeded,
        };
        assert_eq!(
            job.took_at(Timestamp::at_millis(6_000))
                .unwrap()
                .to_string(),
            "3s"
        );
    }

    #[test]
    fn only_missing_content_refusals_request_a_submission_retry() {
        let refused = |code| {
            Reply::Refused(Refusal {
                code,
                detail: RemoteText::new("refused".into()),
            })
        };
        assert!(refused(RefusalCode::MissingContent).needs_content_retry());
        assert!(!refused(RefusalCode::BadRequest).needs_content_retry());
        assert!(!Reply::Stream.needs_content_retry());
    }

    fn assert_phase_decision(
        phase: Phase,
        supervisor: Supervisor,
        expected: (PhaseKind, State, bool, bool),
    ) {
        let job = Job {
            spec: Spec {
                id: "0CCCCCCCCCCCCCCC".parse().unwrap(),
                name: None,
                command: Command::Script("true".into()),
                location: Location::Home,
                env_names: BTreeSet::new(),
                shell: None,
                concurrency: Concurrency::DEFAULT,
                sequence: 0,
                submitted_by: Submitter::Owner,
                submitted_at: Timestamp::at_millis(1),
            },
            phase,
            supervisor,
            behind: Vec::new(),
            notes: Vec::new(),
        };
        assert_eq!(
            (
                job.phase.kind(),
                job.state(),
                job.is_settled(),
                job.succeeded()
            ),
            expected
        );
    }

    #[test]
    fn active_phase_classification_preserves_job_decisions() {
        let started = Timestamp::at_millis(1);
        let starting = Phase::Starting {
            started_at: started,
            workspace: String::new(),
        };
        let running = Phase::Running {
            started_at: started,
            pid: 1,
            workspace: String::new(),
        };
        for (phase, supervisor, expected) in [
            (
                Phase::Queued,
                Supervisor::Alive,
                (PhaseKind::Queued, State::Queued, false, false),
            ),
            (
                Phase::Queued,
                Supervisor::Gone,
                (PhaseKind::Queued, State::RestartPending, false, false),
            ),
            (
                Phase::Preparing {
                    started_at: started,
                },
                Supervisor::Gone,
                (PhaseKind::Preparing, State::RestartPending, false, false),
            ),
            (
                starting.clone(),
                Supervisor::Alive,
                (PhaseKind::Starting, State::Running, false, false),
            ),
            (
                starting,
                Supervisor::Gone,
                (PhaseKind::Starting, State::Lost, true, false),
            ),
            (
                running.clone(),
                Supervisor::Alive,
                (PhaseKind::Running, State::Running, false, false),
            ),
            (
                running,
                Supervisor::Gone,
                (PhaseKind::Running, State::Lost, true, false),
            ),
        ] {
            assert_phase_decision(phase, supervisor, expected);
        }
    }

    #[test]
    fn finished_phase_classification_preserves_job_decisions() {
        let started = Timestamp::at_millis(1);
        let finished_at = Timestamp::at_millis(2);
        for (outcome, supervisor, state, succeeded) in [
            (Outcome::Succeeded, Supervisor::Gone, State::Succeeded, true),
            (
                Outcome::Failed { exit_code: 3 },
                Supervisor::Alive,
                State::Failed,
                false,
            ),
            (Outcome::Killed, Supervisor::Gone, State::Killed, false),
            (
                Outcome::Errored {
                    reason: RemoteText::new("error".into()),
                },
                Supervisor::Gone,
                State::Errored,
                false,
            ),
        ] {
            assert_phase_decision(
                Phase::Finished {
                    started_at: Some(started),
                    finished_at,
                    outcome,
                },
                supervisor,
                (PhaseKind::Finished, state, true, succeeded),
            );
        }
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
