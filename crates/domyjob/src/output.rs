use std::io::Write;

use serde::Serialize;

use crate::diagnosis::{Diagnosis, Kind};
use crate::domain::{JobName, MachineName};
use crate::protocol::{Digest, Found, Job};
use crate::terminal::RemoteText;

pub const SCHEMA: u32 = 3;

pub const DIGEST_TAIL: u32 = 40;

#[derive(Serialize)]
struct Document<'a, T> {
    schema: u32,
    #[serde(flatten)]
    body: &'a T,
}

fn encoded<T: Serialize>(body: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(&Document {
        schema: SCHEMA,
        body,
    })
}

pub fn value<T: Serialize>(body: &T) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(Document {
        schema: SCHEMA,
        body,
    })
}

pub fn print<T: Serialize>(out: &mut dyn Write, body: &T) -> std::io::Result<()> {
    let line = encoded(body).map_err(std::io::Error::other)?;
    writeln!(out, "{line}")
}

#[derive(Debug, Serialize)]
pub struct JobView<'a> {
    job: String,
    machine: &'a MachineName,
    state: &'static str,
    exit_code: Option<i32>,
    name: Option<&'a JobName>,
    command: String,
    reason: Option<&'a RemoteText>,
    notes: &'a [RemoteText],
    behind: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a Job>,
}

impl<'a> JobView<'a> {
    #[must_use]
    pub fn summary(machine: &'a MachineName, job: &'a Job) -> Self {
        Self {
            job: format!("{machine}:{}", job.spec.id),
            machine,
            state: job.state().as_str(),
            exit_code: job.exit_code(),
            name: job.spec.name.as_ref(),
            command: job.spec.command.display(),
            reason: job.reason(),
            notes: &job.notes,
            behind: job
                .behind
                .iter()
                .map(|holder| format!("{machine}:{holder}"))
                .collect(),
            detail: None,
        }
    }

    #[must_use]
    pub fn full(machine: &'a MachineName, job: &'a Job) -> Self {
        Self {
            detail: Some(job),
            ..Self::summary(machine, job)
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DigestView<'a> {
    #[serde(flatten)]
    job: JobView<'a>,
    lines: u64,
    bytes: u64,
    tail: Vec<String>,
}

impl<'a> DigestView<'a> {
    #[must_use]
    pub fn of(machine: &'a MachineName, digest: &'a Digest) -> Self {
        Self {
            job: JobView::summary(machine, &digest.job),
            lines: digest.lines,
            bytes: digest.bytes,
            tail: digest.tail.iter().map(ToString::to_string).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorView {
    kind: Kind,
    message: String,
    hint: Option<String>,
}

impl ErrorView {
    #[must_use]
    pub fn new(message: String, diagnosis: Diagnosis) -> Self {
        Self {
            kind: diagnosis.kind,
            message,
            hint: diagnosis.hint,
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

#[derive(Debug, Serialize)]
pub struct Failed {
    pub error: ErrorView,
}

#[must_use]
pub fn unprefixed(machine: &MachineName, error: &crate::remote::RemoteError) -> String {
    let message = error.to_string();
    match message.strip_prefix(&format!("{machine}: ")) {
        Some(rest) => rest.to_owned(),
        None => message,
    }
}

#[derive(Debug, Serialize)]
pub struct MachineError {
    pub machine: MachineName,
    pub error: ErrorView,
}

impl MachineError {
    #[must_use]
    pub fn of(machine: &MachineName, error: &crate::remote::RemoteError) -> Self {
        Self {
            machine: machine.clone(),
            error: ErrorView::new(
                unprefixed(machine, error),
                crate::diagnosis::of_remote(error).about(Some(machine.as_str())),
            ),
        }
    }

    #[must_use]
    pub fn told(machine: &MachineName, message: String, diagnosis: Diagnosis) -> Self {
        Self {
            machine: machine.clone(),
            error: ErrorView::new(message, diagnosis.about(Some(machine.as_str()))),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Hit {
    line: u64,
    text: String,
    matched: bool,
}

#[derive(Debug, Serialize)]
pub struct FoundView<'a> {
    machine: &'a MachineName,
    matched: u64,
    truncated: bool,
    hits: Vec<Hit>,
}

impl<'a> FoundView<'a> {
    #[must_use]
    pub fn of(machine: &'a MachineName, found: &Found) -> Self {
        Self {
            machine,
            matched: found.matched,
            truncated: found.truncated,
            hits: found
                .hits
                .iter()
                .map(|hit| Hit {
                    line: hit.line,
                    text: hit.text.to_string(),
                    matched: hit.matched,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Jobs<T> {
    pub jobs: Vec<T>,
    pub unreachable: Vec<MachineError>,
}

#[derive(Debug, Serialize)]
pub struct Overview<'a> {
    machine: &'a MachineName,
    report: &'a crate::protocol::Report,
    running: Vec<JobView<'a>>,
    queued: Vec<JobView<'a>>,
    recent: Vec<JobView<'a>>,
}

impl<'a> Overview<'a> {
    #[must_use]
    pub fn of(
        machine: &'a MachineName,
        report: &'a crate::protocol::Report,
        jobs: &'a [Job],
    ) -> Self {
        let summaries = |wanted: &dyn Fn(&Job) -> bool| -> Vec<JobView<'a>> {
            jobs.iter()
                .filter(|job| wanted(job))
                .map(|job| JobView::summary(machine, job))
                .collect()
        };
        Self {
            machine,
            report,
            running: summaries(&|job| {
                matches!(
                    job.state(),
                    crate::protocol::State::Running | crate::protocol::State::Preparing
                )
            }),
            queued: summaries(&|job| job.state() == crate::protocol::State::Queued),
            recent: jobs
                .iter()
                .filter(|job| job.is_settled())
                .take(5)
                .map(|job| JobView::summary(machine, job))
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Answered<T> {
    Answer(T),
    Unreachable(MachineError),
}

#[derive(Debug, Serialize)]
pub struct CleanedOn<'a> {
    pub machine: &'a MachineName,
    pub cleaned: &'a crate::protocol::Cleaned,
}

#[derive(Debug, Serialize)]
pub struct Series<'a> {
    machine: &'a MachineName,
    name: &'a str,
    runs: usize,
    succeeded: usize,
    recent: Vec<&'static str>,
    typical_millis: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct HistoryView<'a> {
    history: Vec<Series<'a>>,
}

impl<'a> HistoryView<'a> {
    #[must_use]
    pub fn of(series: &'a [crate::history::Series]) -> Self {
        Self {
            history: series
                .iter()
                .map(|one| Series {
                    machine: &one.machine,
                    name: &one.label,
                    runs: one.runs,
                    succeeded: one.succeeded,
                    recent: one.recent.iter().map(|state| state.as_str()).collect(),
                    typical_millis: one.typical.map(crate::clock::Elapsed::millis),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Change<'a> {
    pub path: &'a crate::domain::RelPath,
    pub change: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Pulled<'a> {
    pub job: &'a str,
    pub root: &'a std::path::Path,
    pub applied: bool,
    pub changes: Vec<Change<'a>>,
}

#[derive(Debug, Serialize)]
pub struct Configured<'a> {
    pub machine: &'a MachineName,
    pub host: &'a crate::domain::Host,
    pub transport: &'a str,
    pub labels: &'a [String],
    pub facts: Option<crate::remote::Facts>,
}

#[derive(Debug, Serialize)]
pub struct Sending<'a> {
    pub root: &'a std::path::Path,
    pub runs_in: Option<&'a crate::domain::RelPath>,
    pub revision: &'a str,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Preview<'a> {
    pub machines: &'a [MachineName],
    pub command: String,
    pub sending: Option<Sending<'a>>,
}

#[derive(Debug, Serialize)]
pub struct Unsettled {
    pub job: String,
    pub machine: MachineName,
    pub state: &'static str,
    pub error: ErrorView,
}

#[derive(Debug, Serialize)]
pub struct Log {
    pub log: String,
}

#[derive(Debug, Serialize)]
pub struct Fetched<'a> {
    pub machine: &'a MachineName,
    pub path: &'a crate::domain::RelPath,
    pub written: &'a std::path::Path,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Local<'a> {
    pub key_storage: &'a str,
    pub state_fits_local_sockets: bool,
}

#[derive(Debug, Serialize)]
pub struct Checkup<'a, T> {
    pub local: Local<'a>,
    pub machines: &'a [Answered<T>],
    pub notes: &'a [&'a str],
}

#[derive(Debug, Serialize)]
pub struct Machines<T> {
    pub machines: Vec<T>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn every_document_says_its_schema_and_a_job_keeps_its_shape_everywhere() {
        let job = crate::view::tests::sample();
        let machine: MachineName = "linux".parse().unwrap();
        let summary = value(&JobView::summary(&machine, &job)).unwrap();
        assert_eq!(
            keys(&summary),
            [
                "behind",
                "command",
                "exit_code",
                "job",
                "machine",
                "name",
                "notes",
                "reason",
                "schema",
                "state"
            ]
        );
        assert_eq!(summary.get("job").unwrap(), "linux:0AAAAAAAAAAAAAAA");
        let full = value(&JobView::full(&machine, &job)).unwrap();
        assert_eq!(keys(&full).len(), keys(&summary).len() + 1);
        assert!(full.get("detail").is_some());
        let digest = Digest {
            job: job.clone(),
            lines: 2,
            bytes: 10,
            tail: Vec::new(),
        };
        let digested = value(&DigestView::of(&machine, &digest)).unwrap();
        let mut expected = keys(&summary);
        expected.extend(["bytes", "lines", "tail"].map(str::to_owned));
        expected.sort_unstable();
        assert_eq!(keys(&digested), expected);
        let lost = value(&MachineError::told(
            &machine,
            "went silent".to_owned(),
            Diagnosis {
                kind: Kind::Unreachable,
                hint: Some(["domyjob doctor ", "{", "machine}"].concat()),
            },
        ))
        .unwrap();
        assert_eq!(keys(&lost), ["error", "machine", "schema"]);
        assert_eq!(lost.pointer("/error/hint").unwrap(), "domyjob doctor linux");
        assert_eq!(lost.pointer("/error/kind").unwrap(), "unreachable");
        let listed = value(&Jobs {
            jobs: vec![JobView::summary(&machine, &job)],
            unreachable: Vec::new(),
        })
        .unwrap();
        assert!(listed.pointer("/jobs/0/schema").is_none());
        assert_eq!(listed.get("schema").unwrap(), SCHEMA);
    }
}
