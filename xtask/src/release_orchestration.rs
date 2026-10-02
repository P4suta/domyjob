use std::ffi::OsString;
use std::io::Read as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use sha2::Digest as _;

use crate::release_queue::{Handoff, PendingManifest, Source, SourceClaims};

const REPOSITORY: &str = "P4suta/domyjob";
const BUILD_WORKFLOW: &str = ".github/workflows/release.yml";
const FINALIZE_WORKFLOW: &str = ".github/workflows/release-finalize.yml";
const PREDICATE: &str = "https://slsa.dev/provenance/v1";
const LIMIT: usize = 4_194_304;
const SCAN_LIMIT: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum OrchestrationError {
    #[error("release queue I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("release queue JSON: {0}")]
    Json(#[from] domyjob_core::ingress::JsonError),
    #[error("{0}")]
    Invalid(String),
    #[error("release queue validation: {0}")]
    Queue(String),
    #[error("release notarization: {0}")]
    Signing(#[from] crate::release::SigningError),
}

impl From<crate::release_queue::QueueError> for OrchestrationError {
    fn from(error: crate::release_queue::QueueError) -> Self {
        Self::Queue(error.to_string())
    }
}

fn invalid(reason: &str) -> OrchestrationError {
    OrchestrationError::Invalid(reason.to_owned())
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, OrchestrationError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(&format!("missing text field {name}")))
}

fn number(value: &Value, name: &str) -> Result<u64, OrchestrationError> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .filter(|number| *number > 0)
        .ok_or_else(|| invalid(&format!("missing positive integer field {name}")))
}

fn list<'a>(value: &'a Value, name: &str) -> Result<&'a [Value], OrchestrationError> {
    value
        .get(name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(&format!("missing array field {name}")))
}

fn artifact_array(response: &Value, complete: bool) -> Result<&[Value], OrchestrationError> {
    let artifacts = list(response, "artifacts")?;
    let count = response
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("artifact inventory count is missing"))?;
    if artifacts.len() > SCAN_LIMIT || complete && count > 100 {
        return Err(invalid("artifact inventory exceeds its scan bound"));
    }
    Ok(artifacts)
}

fn live_named(artifact: &Value, name: &str) -> bool {
    artifact.get("name").and_then(Value::as_str) == Some(name)
        && artifact.get("expired").and_then(Value::as_bool) == Some(false)
}

fn artifact_digest(artifact: &Value) -> Result<&str, OrchestrationError> {
    let digest = text(artifact, "digest")?;
    if !digest
        .strip_prefix("sha256:")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(invalid("artifact has no SHA-256 digest"));
    }
    Ok(digest)
}

fn exact(value: &Value, name: &str, expected: &str) -> Result<(), OrchestrationError> {
    if text(value, name)? != expected {
        return Err(invalid(&format!("GitHub identity mismatch: {name}")));
    }
    Ok(())
}

fn positive(value: &str) -> Result<u64, OrchestrationError> {
    if value.is_empty()
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(
            "run and artifact IDs must be positive decimal integers",
        ));
    }
    value
        .parse()
        .map_err(|_error| invalid("invalid Actions ID"))
}

fn environment(name: &str) -> Result<String, OrchestrationError> {
    std::env::var(name).map_err(|_error| invalid(&format!("missing environment input {name}")))
}

fn output(name: &str, value: &str) -> Result<(), OrchestrationError> {
    if value.contains(['\r', '\n']) {
        return Err(invalid("GitHub output must occupy one line"));
    }
    crate::raw::append(
        Path::new(&environment("GITHUB_OUTPUT")?),
        format!("{name}={value}\n").as_bytes(),
    )?;
    Ok(())
}

trait Github {
    fn execute(&self, arguments: &[OsString]) -> Result<Vec<u8>, OrchestrationError>;

    fn api(&self, path: &str) -> Result<Value, OrchestrationError> {
        let bytes = self.execute(&[
            "api".into(),
            "--hostname".into(),
            "github.com".into(),
            format!("repos/{REPOSITORY}/{path}").into(),
        ])?;
        Ok(domyjob_core::ingress::json(&bytes, LIMIT)?)
    }
}

mod raw {
    #![expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "release orchestration captures bounded public GitHub metadata and writes public workflow outputs"
    )]

    pub(super) fn read(
        reader: &mut impl std::io::Read,
        bytes: &mut Vec<u8>,
    ) -> std::io::Result<usize> {
        reader.read_to_end(bytes)
    }

    pub(super) fn now() -> std::time::SystemTime {
        std::time::SystemTime::now()
    }
}

fn bounded(mut reader: impl std::io::Read) -> Result<Vec<u8>, OrchestrationError> {
    let mut bytes = Vec::new();
    raw::read(&mut reader.by_ref().take(4_194_305), &mut bytes)?;
    std::io::copy(&mut reader, &mut std::io::sink())?;
    if bytes.len() > LIMIT {
        return Err(invalid("GitHub command output exceeds its bound"));
    }
    Ok(bytes)
}

struct NativeGithub;

impl Github for NativeGithub {
    fn execute(&self, arguments: &[OsString]) -> Result<Vec<u8>, OrchestrationError> {
        let mut command = crate::raw::command("gh");
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in crate::release::SECRET_ENVIRONMENT {
            command.env_remove(name);
        }
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("GitHub output pipe is missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| invalid("GitHub diagnostic pipe is missing"))?;
        std::thread::scope(|scope| {
            let stdout = scope.spawn(|| bounded(stdout));
            let stderr = scope.spawn(|| bounded(stderr));
            let status = child.wait()?;
            let bytes = stdout
                .join()
                .map_err(|_panic| invalid("GitHub output reader stopped"))??;
            stderr
                .join()
                .map_err(|_panic| invalid("GitHub diagnostic reader stopped"))??;
            if !status.success() {
                return Err(invalid(&format!("GitHub command exited with {status}")));
            }
            Ok(bytes)
        })
    }
}

#[derive(Debug)]
struct Origin {
    source: Source,
    artifact_id: u64,
}

#[derive(Debug)]
struct VerifiedHandoff<'a> {
    handoff: &'a Handoff,
    manifest_sha256: String,
}

#[derive(Debug)]
enum VerifiedDistribution<'a> {
    Legacy(&'a VerifiedHandoff<'a>),
    Packages(&'a VerifiedReady<'a>),
}

#[derive(Debug)]
struct ReviewedDistribution<'a> {
    verified: &'a VerifiedDistribution<'a>,
    audit: crate::dependencies::AuditedLockfile,
    lock_sha256: String,
}

impl<'a> ReviewedDistribution<'a> {
    fn new(
        root: &Path,
        github: &impl Github,
        verified: &'a VerifiedDistribution<'a>,
    ) -> Result<Self, OrchestrationError> {
        verified.unchanged()?;
        let lock = source_lockfile(github, verified.source())?;
        let audit = crate::dependencies::audit_snapshot(root, &lock)
            .map_err(|error| invalid(&format!("original release dependency audit: {error}")))?;
        Self::bind(verified, audit, &lock)
    }

    fn bind(
        verified: &'a VerifiedDistribution<'a>,
        audit: crate::dependencies::AuditedLockfile,
        lock: &[u8],
    ) -> Result<Self, OrchestrationError> {
        let reviewed = Self {
            verified,
            audit,
            lock_sha256: data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(lock)),
        };
        reviewed.unchanged()?;
        Ok(reviewed)
    }

    const fn source(&self) -> &Source {
        self.verified.source()
    }

    fn files(&self) -> Result<Vec<(String, std::path::PathBuf, String)>, OrchestrationError> {
        self.verified.files()
    }

    fn unchanged(&self) -> Result<(), OrchestrationError> {
        if self.audit.digest() != self.lock_sha256 {
            return Err(invalid(
                "dependency audit belongs to another lockfile snapshot",
            ));
        }
        self.verified.unchanged()
    }
}

fn source_lockfile(github: &impl Github, source: &Source) -> Result<Vec<u8>, OrchestrationError> {
    let response = github.api(&format!("contents/Cargo.lock?ref={}", source.source_sha()))?;
    for (name, expected) in [
        ("name", "Cargo.lock"),
        ("path", "Cargo.lock"),
        ("type", "file"),
        ("encoding", "base64"),
    ] {
        exact(&response, name, expected)?;
    }
    let size = number(&response, "size")?;
    if size > 1_048_576 {
        return Err(invalid(
            "original release lockfile exceeds its content bound",
        ));
    }
    let content: Vec<_> = text(&response, "content")?
        .bytes()
        .filter(|byte| *byte != b'\n')
        .collect();
    let bytes = data_encoding::BASE64
        .decode(&content)
        .map_err(|_error| invalid("original release lockfile is not canonical base64"))?;
    let expected_size = usize::try_from(size)
        .map_err(|_error| invalid("original release lockfile size is not representable"))?;
    if bytes.len() != expected_size {
        return Err(invalid(
            "original release lockfile content differs from its size",
        ));
    }
    Ok(bytes)
}

#[derive(Debug)]
struct VerifiedReady<'a> {
    pending: &'a VerifiedHandoff<'a>,
    ready: &'a crate::release_ready::ReadyDistribution,
}

impl<'a> VerifiedReady<'a> {
    fn new(
        github: &impl Github,
        pending: &'a VerifiedHandoff<'a>,
        ready: &'a crate::release_ready::ReadyDistribution,
    ) -> Result<Self, OrchestrationError> {
        verify_ready(github, ready, pending.handoff)?;
        let verified = Self { pending, ready };
        verified.unchanged()?;
        Ok(verified)
    }

    fn unchanged(&self) -> Result<(), OrchestrationError> {
        self.pending.unchanged()?;
        self.ready.unchanged(self.pending.handoff)
    }
}

impl VerifiedDistribution<'_> {
    const fn source(&self) -> &Source {
        match self {
            Self::Legacy(pending) => pending.handoff.source(),
            Self::Packages(verified) => verified.ready.source(),
        }
    }

    fn files(&self) -> Result<Vec<(String, std::path::PathBuf, String)>, OrchestrationError> {
        match self {
            Self::Legacy(pending) => release_assets(pending.handoff),
            Self::Packages(verified) => verified.ready.files(),
        }
    }

    fn unchanged(&self) -> Result<(), OrchestrationError> {
        match self {
            Self::Legacy(pending) => {
                if pending
                    .handoff
                    .receipt(crate::release_queue::MAC_TARGETS[0])?
                    .package()
                    .is_some()
                {
                    return Err(invalid(
                        "package publication requires verified staple and derivation provenance",
                    ));
                }
                pending.unchanged()
            }
            Self::Packages(verified) => verified.unchanged(),
        }
    }
}

impl<'a> VerifiedHandoff<'a> {
    fn new(github: &impl Github, handoff: &'a Handoff) -> Result<Self, OrchestrationError> {
        let path = handoff.dir().join("manifest.json");
        let manifest_sha256 = crate::release_queue::sha256_file_bounded(&path)?;
        verify_provenance(github, &path, handoff.source(), &manifest_sha256)?;
        for (_name, archive_path, hash) in handoff.archives() {
            verify_provenance(github, archive_path, handoff.source(), hash)?;
        }
        let verified = Self {
            handoff,
            manifest_sha256,
        };
        verified.unchanged()?;
        Ok(verified)
    }

    fn unchanged(&self) -> Result<(), OrchestrationError> {
        if crate::release_queue::sha256_file_bounded(&self.handoff.dir().join("manifest.json"))?
            != self.manifest_sha256
        {
            return Err(invalid(
                "attested pending manifest changed during finalization",
            ));
        }
        Handoff::inspect(self.handoff.dir(), self.handoff.source())?;
        Ok(())
    }
}

fn repositories(run: &Value) -> Result<(), OrchestrationError> {
    for name in ["repository", "head_repository"] {
        let repository = run
            .get(name)
            .ok_or_else(|| invalid("Actions repository is missing"))?;
        exact(repository, "full_name", REPOSITORY)?;
    }
    Ok(())
}

fn original_run(run: &Value, workflow_id: u64) -> Result<(), OrchestrationError> {
    repositories(run)?;
    exact(run, "path", BUILD_WORKFLOW)?;
    exact(run, "status", "completed")?;
    exact(run, "conclusion", "success")?;
    if number(run, "workflow_id")? != workflow_id {
        return Err(invalid("original workflow ID differs"));
    }
    match text(run, "event")? {
        "workflow_dispatch" => exact(run, "head_branch", "main"),
        "push" if text(run, "head_branch")?.starts_with('v') => Ok(()),
        _ => Err(invalid(
            "original run is not a trusted main rehearsal or version tag push",
        )),
    }
}

fn workflow_id(
    github: &impl Github,
    filename: &str,
    expected: &str,
) -> Result<u64, OrchestrationError> {
    let workflow = github.api(&format!("actions/workflows/{filename}"))?;
    exact(&workflow, "path", expected)?;
    exact(&workflow, "state", "active")?;
    number(&workflow, "id")
}

fn timestamp(value: &str) -> Result<u64, OrchestrationError> {
    let digits = |range: std::ops::Range<usize>| -> Result<u64, OrchestrationError> {
        let field = value
            .get(range)
            .ok_or_else(|| invalid("invalid Actions UTC timestamp"))?;
        if !field.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid("invalid Actions UTC timestamp"));
        }
        field
            .parse()
            .map_err(|_error| invalid("invalid Actions UTC timestamp"))
    };
    if value.len() != 20
        || value.get(4..5) != Some("-")
        || value.get(7..8) != Some("-")
        || value.get(10..11) != Some("T")
        || value.get(13..14) != Some(":")
        || value.get(16..17) != Some(":")
        || value.get(19..20) != Some("Z")
    {
        return Err(invalid("invalid Actions UTC timestamp"));
    }
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    let leap = |candidate: u64| {
        candidate.is_multiple_of(4)
            && (!candidate.is_multiple_of(100) || candidate.is_multiple_of(400))
    };
    let days = |candidate: u64| match candidate {
        4 | 6 | 9 | 11 => 30,
        2 if leap(year) => 29,
        2 => 28,
        _ => 31,
    };
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day == 0
        || day > days(month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(invalid("invalid Actions UTC timestamp"));
    }
    let preceding_years = (1970..year)
        .map(|candidate| if leap(candidate) { 366_u64 } else { 365 })
        .sum::<u64>();
    let preceding_months = (1..month).map(days).sum::<u64>();
    Ok(preceding_years
        .saturating_add(preceding_months)
        .saturating_add(day.saturating_sub(1))
        .saturating_mul(86_400)
        .saturating_add(hour.saturating_mul(3600))
        .saturating_add(minute.saturating_mul(60))
        .saturating_add(second))
}

fn active_age(run: &Value, now: u64) -> Result<bool, OrchestrationError> {
    let created = timestamp(text(run, "created_at")?)?;
    Ok(
        crate::release_queue::NotaryState::InProgress.with_deadline(created, now)?
            != crate::release_queue::NotaryState::Expired,
    )
}

fn pending_artifact(
    github: &impl Github,
    run: &Value,
    require_live: bool,
) -> Result<Option<u64>, OrchestrationError> {
    let run_id = number(run, "id")?;
    let artifacts = github.api(&format!("actions/runs/{run_id}/artifacts?per_page=100"))?;
    let all = artifact_array(&artifacts, true)?;
    let mut found = None;
    for artifact in all {
        if text(artifact, "name")? != "dist-pending" {
            continue;
        }
        let expired = artifact
            .get("expired")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid("pending artifact expiration state is missing"))?;
        if require_live && expired {
            return Err(invalid("pending distribution artifact expired"));
        }
        let artifact_run = artifact
            .get("workflow_run")
            .ok_or_else(|| invalid("artifact run identity is missing"))?;
        if number(artifact_run, "id")? != run_id
            || text(artifact_run, "head_sha")? != text(run, "head_sha")?
        {
            return Err(invalid("pending artifact belongs to another run or source"));
        }
        artifact_digest(artifact)?;
        if found.replace(number(artifact, "id")?).is_some() {
            return Err(invalid("multiple pending distribution artifacts"));
        }
    }
    Ok(found)
}

fn completed(
    github: &impl Github,
    run_id: u64,
    attempt: u64,
    workflow: u64,
) -> Result<bool, OrchestrationError> {
    let name = format!("dist-verified-{run_id}-{attempt}");
    let artifacts = github.api(&format!("actions/artifacts?name={name}&per_page=100"))?;
    let markers = artifact_array(&artifacts, false)?;
    for artifact in markers {
        if !live_named(artifact, &name) {
            continue;
        }
        let Some(id) = artifact
            .get("workflow_run")
            .and_then(|run| match number(run, "id") {
                Ok(id) => Some(id),
                Err(_untrusted_marker) => None,
            })
        else {
            continue;
        };
        let run = github.api(&format!("actions/runs/{id}"))?;
        if trusted_completion(&run, id, workflow).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn trusted_completion(run: &Value, id: u64, workflow: u64) -> Result<(), OrchestrationError> {
    finalizer_identity(run, workflow)?;
    if number(run, "id")? != id {
        return Err(invalid(
            "completion marker is not from the trusted finalizer",
        ));
    }
    exact(run, "status", "completed")?;
    exact(run, "conclusion", "success")
}

#[derive(Clone, Copy)]
struct Discovery<'a> {
    event: &'a str,
    requested: &'a str,
    trigger: &'a str,
}

fn discover(
    github: &impl Github,
    input: Discovery<'_>,
    now: u64,
) -> Result<Value, OrchestrationError> {
    let Discovery {
        event,
        requested,
        trigger,
    } = input;
    let workflow = workflow_id(github, "release.yml", BUILD_WORKFLOW)?;
    let finalizer = workflow_id(github, "release-finalize.yml", FINALIZE_WORKFLOW)?;
    let explicit = match event {
        "workflow_dispatch" if !requested.is_empty() => Some(positive(requested)?),
        "workflow_dispatch" | "schedule" if requested.is_empty() && trigger.is_empty() => None,
        "workflow_run" if requested.is_empty() => Some(positive(trigger)?),
        _ => return Err(invalid("unsupported release queue discovery input")),
    };
    let runs = if let Some(id) = explicit {
        vec![github.api(&format!("actions/runs/{id}"))?]
    } else {
        let response = github.api(&format!(
            "actions/workflows/{workflow}/runs?status=completed&per_page=100"
        ))?;
        let runs = list(&response, "workflow_runs")?;
        if runs.len() > SCAN_LIMIT {
            return Err(invalid("original run list exceeds its scan bound"));
        }
        runs.to_vec()
    };
    let mut pending = Vec::new();
    let mut expired = Vec::new();
    for run in runs {
        if original_run(&run, workflow).is_err() {
            if explicit.is_some() && event == "workflow_dispatch" {
                return Err(invalid("requested original run is not eligible"));
            }
            continue;
        }
        let active = active_age(&run, now)?;
        let Some(artifact) = pending_artifact(github, &run, active)? else {
            if explicit.is_some() && event == "workflow_dispatch" {
                return Err(invalid("requested run has no pending distribution"));
            }
            continue;
        };
        let id = number(&run, "id")?;
        let attempt = number(&run, "run_attempt")?;
        if completed(github, id, attempt, finalizer)? {
            continue;
        }
        if !active {
            if explicit.is_some() && event == "workflow_dispatch" {
                return Err(invalid(
                    "requested notarization queue exceeded its seven-day deadline",
                ));
            }
            expired.push(id);
            continue;
        }
        pending.push(json!({"run_id":id.to_string(),"run_attempt":attempt.to_string(),"artifact_id":artifact.to_string()}));
    }
    Ok(json!({"include":pending,"expiredRunIds":expired,"scanLimit":SCAN_LIMIT}))
}

fn validate_origin(
    github: &impl Github,
    source: Source,
    artifact_id: u64,
    now: u64,
) -> Result<Origin, OrchestrationError> {
    let workflow = workflow_id(github, "release.yml", BUILD_WORKFLOW)?;
    let run = github.api(&format!(
        "actions/runs/{}/attempts/{}",
        source.origin_run_id(),
        source.run_attempt()
    ))?;
    original_run(&run, workflow)?;
    if number(&run, "id")? != source.origin_run_id()
        || number(&run, "run_attempt")? != source.run_attempt()
        || text(&run, "head_sha")? != source.source_sha()
        || text(&run, "event")? != source.event()
    {
        return Err(invalid(
            "pending source differs from its original run attempt",
        ));
    }
    let expected_branch = source
        .source_ref()
        .strip_prefix("refs/heads/")
        .or_else(|| source.source_ref().strip_prefix("refs/tags/"))
        .ok_or_else(|| invalid("unsupported pending source ref"))?;
    exact(&run, "head_branch", expected_branch)?;
    if !active_age(&run, now)? {
        return Err(invalid(
            "notarization queue exceeded its seven-day deadline",
        ));
    }
    if pending_artifact(github, &run, true)? != Some(artifact_id) {
        return Err(invalid("pending artifact ID differs from its original run"));
    }
    main_ancestry(github, &source)?;
    Ok(Origin {
        source,
        artifact_id,
    })
}

fn main_ancestry(github: &impl Github, source: &Source) -> Result<(), OrchestrationError> {
    let reference = github.api("git/ref/heads/main")?;
    exact(&reference, "ref", "refs/heads/main")?;
    let object = reference
        .get("object")
        .ok_or_else(|| invalid("main commit object is missing"))?;
    exact(object, "type", "commit")?;
    let main = text(object, "sha")?;
    if main.len() != 40 || !main.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("main commit hash is invalid"));
    }
    let comparison = github.api(&format!("compare/{}...{main}", source.source_sha()))?;
    if !matches!(text(&comparison, "status")?, "identical" | "ahead")
        || comparison
            .get("merge_base_commit")
            .and_then(|commit| commit.get("sha"))
            .and_then(Value::as_str)
            != Some(source.source_sha())
    {
        return Err(invalid(
            "original source is not an ancestor of current main",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn provenance(entries: &Value, source: &Source, hash: &str) -> Result<(), OrchestrationError> {
    provenance_for(entries, source, hash, BUILD_WORKFLOW)
}

fn provenance_for(
    entries: &Value,
    source: &Source,
    hash: &str,
    signer_workflow: &str,
) -> Result<(), OrchestrationError> {
    let entries = entries
        .as_array()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| invalid("no verified attestations"))?;
    let workflow = format!(
        "https://github.com/{REPOSITORY}/{signer_workflow}@{}",
        source.source_ref()
    );
    let invocation = format!(
        "https://github.com/{REPOSITORY}/actions/runs/{}/attempts/{}",
        source.origin_run_id(),
        source.run_attempt()
    );
    let repository = format!("https://github.com/{REPOSITORY}");
    for entry in entries {
        let result = entry
            .get("verificationResult")
            .ok_or_else(|| invalid("missing verified attestation result"))?;
        let certificate = result
            .get("signature")
            .and_then(|value| value.get("certificate"))
            .ok_or_else(|| invalid("missing attestation certificate"))?;
        let required = [
            ("issuer", "https://token.actions.githubusercontent.com"),
            ("sourceRepositoryURI", repository.as_str()),
            ("sourceRepositoryDigest", source.source_sha()),
            ("sourceRepositoryRef", source.source_ref()),
            ("buildSignerURI", workflow.as_str()),
            ("buildSignerDigest", source.source_sha()),
            ("runnerEnvironment", "github-hosted"),
            ("runInvocationURI", invocation.as_str()),
        ];
        if required.iter().any(|(name, expected)| {
            certificate.get(name).and_then(Value::as_str) != Some(*expected)
        }) {
            continue;
        }
        let statement = result
            .get("statement")
            .ok_or_else(|| invalid("missing provenance statement"))?;
        exact(statement, "predicateType", PREDICATE)?;
        if list(statement, "subject")?.iter().any(|subject| {
            subject
                .get("digest")
                .and_then(|value| value.get("sha256"))
                .and_then(Value::as_str)
                == Some(hash)
        }) && !list(result, "verifiedTimestamps")?.is_empty()
        {
            return Ok(());
        }
    }
    Err(invalid(
        "no attestation binds this artifact to the exact original source, workflow, and attempt",
    ))
}

fn verify_provenance(
    github: &impl Github,
    file: &Path,
    source: &Source,
    hash: &str,
) -> Result<(), OrchestrationError> {
    verify_provenance_for(
        github,
        AttestationSubject {
            file,
            source,
            hash,
            workflow: BUILD_WORKFLOW,
        },
    )
}

#[derive(Clone, Copy)]
struct AttestationSubject<'a> {
    file: &'a Path,
    source: &'a Source,
    hash: &'a str,
    workflow: &'a str,
}

fn verify_provenance_for(
    github: &impl Github,
    subject: AttestationSubject<'_>,
) -> Result<(), OrchestrationError> {
    let AttestationSubject {
        file,
        source,
        hash,
        workflow: signer_workflow,
    } = subject;
    let bytes = github.execute(&[
        "attestation".into(),
        "verify".into(),
        file.as_os_str().to_owned(),
        "--hostname".into(),
        "github.com".into(),
        "--repo".into(),
        REPOSITORY.into(),
        "--signer-workflow".into(),
        format!("{REPOSITORY}/{signer_workflow}").into(),
        "--source-digest".into(),
        source.source_sha().into(),
        "--source-ref".into(),
        source.source_ref().into(),
        "--signer-digest".into(),
        source.source_sha().into(),
        "--deny-self-hosted-runners".into(),
        "--predicate-type".into(),
        PREDICATE.into(),
        "--digest-alg".into(),
        "sha256".into(),
        "--format".into(),
        "json".into(),
    ])?;
    provenance_for(
        &domyjob_core::ingress::json(&bytes, LIMIT)?,
        source,
        hash,
        signer_workflow,
    )
}

fn tag_binding(
    github: &impl Github,
    source: &Source,
) -> Result<Option<String>, OrchestrationError> {
    let Some(tag) = source.publish_tag() else {
        return Ok(None);
    };
    let tag = tag.to_owned();
    if source.event() != "push" || source.source_ref() != format!("refs/tags/{tag}") {
        return Err(invalid(
            "publication requires the original version tag push",
        ));
    }
    let reference = github.api(&format!("git/ref/tags/{tag}"))?;
    exact(&reference, "ref", source.source_ref())?;
    let mut object = reference
        .get("object")
        .ok_or_else(|| invalid("tag object is missing"))?
        .clone();
    for _ in 0..4 {
        match text(&object, "type")? {
            "commit" => {
                exact(&object, "sha", source.source_sha())?;
                protected_tag(github, source.source_ref())?;
                return Ok(Some(tag));
            }
            "tag" => {
                let hash = text(&object, "sha")?;
                if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(invalid("invalid annotated tag hash"));
                }
                let annotated = github.api(&format!("git/tags/{hash}"))?;
                object = annotated
                    .get("object")
                    .ok_or_else(|| invalid("annotated tag object is missing"))?
                    .clone();
            }
            _ => return Err(invalid("version tag does not point to a commit")),
        }
    }
    Err(invalid("annotated tag chain exceeds its bound"))
}

fn protects_reference(ruleset: &Value, reference: &str) -> Result<bool, OrchestrationError> {
    if text(ruleset, "target")? != "tag" || text(ruleset, "enforcement")? != "active" {
        return Ok(false);
    }
    exact(ruleset, "source_type", "Repository")?;
    exact(ruleset, "source", REPOSITORY)?;
    if ruleset.get("bypass_actors").is_some() && !list(ruleset, "bypass_actors")?.is_empty() {
        return Ok(false);
    }
    let names = ruleset
        .get("conditions")
        .and_then(|conditions| conditions.get("ref_name"))
        .ok_or_else(|| invalid("tag ruleset ref conditions are missing"))?;
    if !list(names, "exclude")?.is_empty() {
        return Ok(false);
    }
    let included = list(names, "include")?.iter().any(|value| {
        value.as_str().is_some_and(|pattern| {
            pattern == reference
                || pattern == "refs/tags/v*" && reference.starts_with("refs/tags/v")
        })
    });
    let rules = list(ruleset, "rules")?;
    Ok(included
        && ["deletion", "update"].iter().all(|required| {
            rules
                .iter()
                .any(|rule| rule.get("type").and_then(Value::as_str) == Some(*required))
        }))
}

fn bypass_query(id: u64) -> String {
    format!(
        "query {{ repository(owner: \"P4suta\", name: \"domyjob\") {{ nameWithOwner ruleset(databaseId: {id}) {{ id databaseId target enforcement source {{ __typename ... on Repository {{ nameWithOwner }} }} bypassActors(first: 1) {{ totalCount nodes {{ id }} pageInfo {{ hasNextPage }} }} }} }} }}"
    )
}

fn bypass_count(response: &Value, ruleset: &Value) -> Result<u64, OrchestrationError> {
    if let Some(errors) = response.get("errors")
        && !errors.as_array().is_some_and(Vec::is_empty)
    {
        return Err(invalid("ruleset GraphQL query reported errors"));
    }
    let repository = response
        .pointer("/data/repository")
        .ok_or_else(|| invalid("ruleset GraphQL repository is missing"))?;
    exact(repository, "nameWithOwner", REPOSITORY)?;
    let graphql = repository
        .get("ruleset")
        .ok_or_else(|| invalid("ruleset GraphQL metadata is missing"))?;
    if number(graphql, "databaseId")? != number(ruleset, "id")? {
        return Err(invalid("ruleset GraphQL database ID differs"));
    }
    exact(graphql, "id", text(ruleset, "node_id")?)?;
    exact(graphql, "target", "TAG")?;
    exact(graphql, "enforcement", "ACTIVE")?;
    let source = graphql
        .get("source")
        .ok_or_else(|| invalid("ruleset GraphQL source is missing"))?;
    exact(source, "__typename", "Repository")?;
    exact(source, "nameWithOwner", REPOSITORY)?;
    let actors = graphql
        .get("bypassActors")
        .ok_or_else(|| invalid("ruleset GraphQL bypass connection is missing"))?;
    let count = actors
        .get("totalCount")
        .and_then(Value::as_u64)
        .filter(|count| *count <= 2_147_483_647)
        .ok_or_else(|| invalid("ruleset GraphQL bypass count is missing"))?;
    let nodes = list(actors, "nodes")?;
    if nodes.len() != usize::from(count > 0)
        || actors
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            != Some(count > 1)
        || nodes
            .iter()
            .any(|node| !node.is_null() && !text(node, "id").is_ok_and(|id| !id.is_empty()))
    {
        return Err(invalid(
            "ruleset GraphQL bypass count contradicts its first page",
        ));
    }
    if ruleset.get("bypass_actors").is_some()
        && u64::try_from(list(ruleset, "bypass_actors")?.len())
            .map_err(|_error| invalid("ruleset REST bypass count exceeds its bound"))?
            != count
    {
        return Err(invalid("ruleset REST and GraphQL bypass counts differ"));
    }
    Ok(count)
}

fn protected_tag(github: &impl Github, reference: &str) -> Result<(), OrchestrationError> {
    let response = github.api("rulesets?includes_parents=false&per_page=100")?;
    let rulesets = response
        .as_array()
        .filter(|rulesets| rulesets.len() < SCAN_LIMIT)
        .ok_or_else(|| invalid("tag ruleset scan exceeds its bound"))?;
    for ruleset in rulesets {
        if text(ruleset, "target")? != "tag" || text(ruleset, "enforcement")? != "active" {
            continue;
        }
        let id = number(ruleset, "id")?;
        if id > 2_147_483_647 {
            return Err(invalid("ruleset database ID exceeds its GraphQL bound"));
        }
        let details = github.api(&format!("rulesets/{id}"))?;
        if number(&details, "id")? != id {
            return Err(invalid("ruleset REST database ID differs"));
        }
        if !protects_reference(&details, reference)? {
            continue;
        }
        let bytes = github.execute(&[
            "api".into(),
            "--hostname".into(),
            "github.com".into(),
            "graphql".into(),
            "-f".into(),
            format!("query={}", bypass_query(id)).into(),
        ])?;
        if bypass_count(&domyjob_core::ingress::json(&bytes, LIMIT)?, &details)? == 0 {
            return Ok(());
        }
    }
    Err(invalid(
        "version tag lacks active update and deletion protection without bypass actors",
    ))
}

fn policy(github: &impl Github, version: &str) -> Result<(), OrchestrationError> {
    if version.len() > 128 {
        return Err(invalid("policy version exceeds its bound"));
    }
    let parsed = semver::Version::parse(version)
        .map_err(|error| invalid(&format!("policy version must be valid SemVer: {error}")))?;
    if parsed.to_string() != version {
        return Err(invalid("policy version must use canonical bounded SemVer"));
    }
    protected_tag(github, &format!("refs/tags/v{version}"))
}

fn handoff(root: &Path) -> Result<(), OrchestrationError> {
    let source = Source::new(SourceClaims {
        source_sha: environment("GITHUB_SHA")?,
        origin_run_id: positive(&environment("GITHUB_RUN_ID")?)?,
        run_attempt: positive(&environment("GITHUB_RUN_ATTEMPT")?)?,
        source_ref: environment("GITHUB_REF")?,
        event: environment("GITHUB_EVENT_NAME")?,
        version: environment("VERSION")?,
    })?;
    crate::release_queue::create_handoff(&root.join("dist"), &source)?;
    Ok(())
}

fn selected_origin(
    root: &Path,
    github: &impl Github,
    now: u64,
) -> Result<Origin, OrchestrationError> {
    let dist = root.join("dist");
    let manifest = PendingManifest::load(&dist.join("manifest.json"))?;
    let source = manifest.source().clone();
    if source.origin_run_id() != positive(&environment("QUEUE_RUN_ID")?)?
        || source.run_attempt() != positive(&environment("QUEUE_RUN_ATTEMPT")?)?
    {
        return Err(invalid(
            "downloaded manifest differs from the selected queue entry",
        ));
    }
    validate_origin(
        github,
        source,
        positive(&environment("QUEUE_ARTIFACT_ID")?)?,
        now,
    )
}

fn finalize(root: &Path, github: &impl Github, now: u64) -> Result<(), OrchestrationError> {
    let dist = root.join("dist");
    let origin = selected_origin(root, github, now)?;
    let handoff = Handoff::inspect(&dist, &origin.source)?;
    let finalizer = workflow_id(github, "release-finalize.yml", FINALIZE_WORKFLOW)?;
    if completed(
        github,
        origin.source.origin_run_id(),
        origin.source.run_attempt(),
        finalizer,
    )? {
        return output("accepted", "false");
    }
    let verified = VerifiedHandoff::new(github, &handoff)?;
    let modern = handoff
        .receipt(crate::release_queue::MAC_TARGETS[0])?
        .package()
        .is_some();
    let accepted = finish(crate::release::notary_status(&handoff)?, |token| {
        if modern {
            prepare_ready(root, github, &verified, token)
        } else {
            Publisher { root, github }.publish(
                &origin,
                &VerifiedDistribution::Legacy(&verified),
                token,
            )
        }
    })?;
    verified.unchanged()?;
    output("modern", if modern { "true" } else { "false" })?;
    output(
        "distribution_path",
        if modern { "ready/*" } else { "dist/*" },
    )?;
    output("accepted", if accepted { "true" } else { "false" })
}

fn producer(version: &str) -> Result<crate::release_ready::Finalizer, OrchestrationError> {
    crate::release_ready::Finalizer::try_from(SourceClaims {
        source_sha: environment("GITHUB_SHA")?,
        origin_run_id: positive(&environment("GITHUB_RUN_ID")?)?,
        run_attempt: positive(&environment("GITHUB_RUN_ATTEMPT")?)?,
        source_ref: "refs/heads/main".to_owned(),
        event: environment("GITHUB_EVENT_NAME")?,
        version: version.to_owned(),
    })
}

fn finalizer_identity(run: &Value, workflow: u64) -> Result<(), OrchestrationError> {
    repositories(run)?;
    if number(run, "workflow_id")? != workflow
        || text(run, "path")? != FINALIZE_WORKFLOW
        || text(run, "head_branch")? != "main"
        || !matches!(
            text(run, "event")?,
            "schedule" | "workflow_run" | "workflow_dispatch"
        )
    {
        return Err(invalid("ready artifact is not from the trusted finalizer"));
    }
    Source::new(SourceClaims {
        source_sha: text(run, "head_sha")?.to_owned(),
        origin_run_id: number(run, "id")?,
        run_attempt: number(run, "run_attempt")?,
        source_ref: "refs/heads/main".to_owned(),
        event: "workflow_dispatch".to_owned(),
        version: "0.0.0".to_owned(),
    })?;
    Ok(())
}

struct ReadyArtifact {
    run_id: u64,
    artifact_id: u64,
    digest: String,
}

fn saved_ready_run(
    github: &impl Github,
    source: &Source,
) -> Result<Option<ReadyArtifact>, OrchestrationError> {
    let name = format!(
        "dist-ready-{}-{}",
        source.origin_run_id(),
        source.run_attempt()
    );
    let response = github.api(&format!("actions/artifacts?name={name}&per_page=100"))?;
    let artifacts = artifact_array(&response, true)?;
    let workflow = workflow_id(github, "release-finalize.yml", FINALIZE_WORKFLOW)?;
    let mut selected: Option<ReadyArtifact> = None;
    for artifact in artifacts {
        if !live_named(artifact, &name) {
            continue;
        }
        let run_id = number(
            artifact
                .get("workflow_run")
                .ok_or_else(|| invalid("ready artifact run is missing"))?,
            "id",
        )?;
        let run = github.api(&format!("actions/runs/{run_id}"))?;
        if finalizer_identity(&run, workflow).is_err() {
            continue;
        }
        let digest = artifact_digest(artifact)?;
        if selected.as_ref().is_none_or(|old| old.run_id < run_id) {
            selected = Some(ReadyArtifact {
                run_id,
                artifact_id: number(artifact, "id")?,
                digest: digest.to_owned(),
            });
        }
    }
    Ok(selected)
}

fn ready_artifact_binding(
    github: &impl Github,
    source: &Source,
    selected: &ReadyArtifact,
) -> Result<(), OrchestrationError> {
    let response = github.api(&format!(
        "actions/runs/{}/artifacts?per_page=100",
        selected.run_id
    ))?;
    let name = format!(
        "dist-ready-{}-{}",
        source.origin_run_id(),
        source.run_attempt()
    );
    let artifacts = artifact_array(&response, true)?;
    let matching: Vec<_> = artifacts
        .iter()
        .filter(|artifact| artifact.get("name").and_then(Value::as_str) == Some(name.as_str()))
        .collect();
    let [artifact] = matching.as_slice() else {
        return Err(invalid(
            "saved ready run needs exactly one artifact with its original name",
        ));
    };
    if number(artifact, "id")? != selected.artifact_id
        || text(artifact, "digest")? != selected.digest
        || artifact.get("expired").and_then(Value::as_bool) != Some(false)
    {
        return Err(invalid("saved ready artifact was replaced or expired"));
    }
    Ok(())
}

fn verify_ready(
    github: &impl Github,
    ready: &crate::release_ready::ReadyDistribution,
    handoff: &Handoff,
) -> Result<(), OrchestrationError> {
    let producer = ready.producer().source();
    let run = github.api(&format!(
        "actions/runs/{}/attempts/{}",
        producer.origin_run_id(),
        producer.run_attempt()
    ))?;
    let workflow = workflow_id(github, "release-finalize.yml", FINALIZE_WORKFLOW)?;
    if number(&run, "id")? != producer.origin_run_id()
        || number(&run, "run_attempt")? != producer.run_attempt()
        || text(&run, "head_sha")? != producer.source_sha()
        || text(&run, "event")? != ready.producer().event()
    {
        return Err(invalid(
            "ready derivation producer differs from its original attempt",
        ));
    }
    finalizer_identity(&run, workflow)?;
    main_ancestry(github, producer)?;
    let comparison = github.api(&format!(
        "compare/{}...{}",
        handoff.source().source_sha(),
        producer.source_sha()
    ))?;
    if !matches!(text(&comparison, "status")?, "identical" | "ahead")
        || text(
            comparison
                .get("merge_base_commit")
                .ok_or_else(|| invalid("derivation ancestry is missing"))?,
            "sha",
        )? != handoff.source().source_sha()
    {
        return Err(invalid(
            "derivation producer predates or diverges from the original build",
        ));
    }
    let (manifest, manifest_hash) = ready.build_manifest();
    verify_provenance(github, &manifest, handoff.source(), &manifest_hash)?;
    for (path, hash) in ready.subjects() {
        verify_provenance_for(
            github,
            AttestationSubject {
                file: &path,
                source: producer,
                hash: &hash,
                workflow: FINALIZE_WORKFLOW,
            },
        )?;
    }
    ready.verify_native(handoff)
}

fn prepare_ready(
    root: &Path,
    github: &impl Github,
    verified: &VerifiedHandoff<'_>,
    accepted: &crate::release::AcceptedToken,
) -> Result<(), OrchestrationError> {
    let handoff = verified.handoff;
    let destination = root.join("ready");
    match std::fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err(invalid("ready output already exists before preparation")),
    }
    if let Some(selected) = saved_ready_run(github, handoff.source())? {
        ready_artifact_binding(github, handoff.source(), &selected)?;
        github.execute(&[
            "run".into(),
            "download".into(),
            selected.run_id.to_string().into(),
            "--repo".into(),
            REPOSITORY.into(),
            "--name".into(),
            format!(
                "dist-ready-{}-{}",
                handoff.source().origin_run_id(),
                handoff.source().run_attempt()
            )
            .into(),
            "--dir".into(),
            destination.as_os_str().to_owned(),
        ])?;
        ready_artifact_binding(github, handoff.source(), &selected)?;
        let ready = crate::release_ready::ReadyDistribution::inspect(&destination, handoff)?;
        verify_ready(github, &ready, handoff)?;
        output("reused_ready", "true")?;
    } else {
        crate::release_ready::ReadyDistribution::create(
            handoff,
            accepted,
            producer(handoff.source().version())?,
            &destination,
        )?;
        output("reused_ready", "false")?;
    }
    verified.unchanged()
}

fn publish_ready(root: &Path, github: &impl Github, now: u64) -> Result<(), OrchestrationError> {
    let origin = selected_origin(root, github, now)?;
    let handoff = Handoff::inspect(&root.join("dist"), &origin.source)?;
    let pending = VerifiedHandoff::new(github, &handoff)?;
    let ready = crate::release_ready::ReadyDistribution::inspect(&root.join("ready"), &handoff)?;
    let proof = VerifiedReady::new(github, &pending, &ready)?;
    let verified = VerifiedDistribution::Packages(&proof);
    if !finish(crate::release::notary_status(&handoff)?, |accepted| {
        Publisher { root, github }.publish(&origin, &verified, accepted)
    })? {
        return Err(invalid("ready distribution lost notarization acceptance"));
    }
    verified.unchanged()
}

fn finish(
    outcome: crate::release::NotaryOutcome,
    publish: impl FnOnce(&crate::release::AcceptedToken) -> Result<(), OrchestrationError>,
) -> Result<bool, OrchestrationError> {
    match outcome {
        crate::release::NotaryOutcome::Pending => Ok(false),
        crate::release::NotaryOutcome::Accepted(token) => {
            publish(&token)?;
            Ok(true)
        }
    }
}

struct Publisher<'a, G> {
    root: &'a Path,
    github: &'a G,
}

impl<G: Github> Publisher<'_, G> {
    fn publish(
        &self,
        origin: &Origin,
        verified: &VerifiedDistribution<'_>,
        accepted: &crate::release::AcceptedToken,
    ) -> Result<(), OrchestrationError> {
        let github = self.github;
        if accepted.source() != &origin.source || verified.source() != &origin.source {
            return Err(invalid(
                "accepted notarization token belongs to another source",
            ));
        }
        let now = unix_now()?;
        validate_origin(github, origin.source.clone(), origin.artifact_id, now)?;
        verified.unchanged()?;
        let Some(tag) = tag_binding(github, &origin.source)? else {
            return Ok(());
        };
        let reviewed = ReviewedDistribution::new(self.root, github, verified)?;
        if let Some(release) = release_for_tag(github, &tag)? {
            resume_publication(github, &tag, &reviewed, &release)?;
            return verify_published(github, &tag, &reviewed);
        }
        create_release(github, &tag, origin, &reviewed)?;
        verify_published(github, &tag, &reviewed)
    }
}

fn release_for_tag(github: &impl Github, tag: &str) -> Result<Option<Value>, OrchestrationError> {
    let releases = github.api("releases?per_page=100")?;
    let releases = releases
        .as_array()
        .filter(|releases| releases.len() < SCAN_LIMIT)
        .ok_or_else(|| invalid("release inventory exceeds its scan bound"))?;
    let matching: Vec<_> = releases
        .iter()
        .filter(|release| release.get("tag_name").and_then(Value::as_str) == Some(tag))
        .collect();
    if matching.len() > 1 {
        return Err(invalid("multiple releases name the original tag"));
    }
    matching
        .first()
        .map(|release| reload_release(github, tag, release))
        .transpose()
}

fn reload_release(
    github: &impl Github,
    tag: &str,
    release: &Value,
) -> Result<Value, OrchestrationError> {
    let id = number(release, "id")?;
    let current = github.api(&format!("releases/{id}"))?;
    if number(&current, "id")? != id {
        return Err(invalid("release identity changed before publication"));
    }
    exact(&current, "tag_name", tag)?;
    Ok(current)
}

fn create_release(
    github: &impl Github,
    tag: &str,
    origin: &Origin,
    verified: &ReviewedDistribution<'_>,
) -> Result<(), OrchestrationError> {
    if verified.source() != &origin.source {
        return Err(invalid("new release belongs to another original source"));
    }
    let mut arguments = vec![
        "release".into(),
        "create".into(),
        tag.into(),
        "--repo".into(),
        REPOSITORY.into(),
        "--title".into(),
        tag.into(),
        "--generate-notes".into(),
        "--verify-tag".into(),
        "--draft".into(),
    ];
    for (_name, path, _hash) in verified.files()? {
        arguments.push(path.into_os_string());
    }
    verified.unchanged()?;
    github.execute(&arguments)?;
    let draft = release_for_tag(github, tag)?
        .ok_or_else(|| invalid("new draft release is missing from the release inventory"))?;
    if draft.get("draft").and_then(Value::as_bool) != Some(true) {
        return Err(invalid("new release was not created as a draft"));
    }
    resume_publication(github, tag, verified, &draft)
}

fn release_assets(
    handoff: &Handoff,
) -> Result<Vec<(String, std::path::PathBuf, String)>, OrchestrationError> {
    let mut assets = Vec::new();
    for (name, path, hash) in handoff.archives() {
        assets.push((name.to_owned(), path.to_path_buf(), hash.to_owned()));
        let checksum = format!("{name}.sha256");
        let checksum_path = handoff.dir().join(&checksum);
        let checksum_hash = crate::release_queue::sha256_file_bounded(&checksum_path)?;
        assets.push((checksum, checksum_path, checksum_hash));
    }
    Ok(assets)
}

fn missing_assets(
    release: &Value,
    expected: &[(String, std::path::PathBuf, String)],
) -> Result<Vec<std::path::PathBuf>, OrchestrationError> {
    let assets = list(release, "assets")?;
    let mut names = std::collections::BTreeSet::new();
    for asset in assets {
        let name = text(asset, "name")?;
        if !names.insert(name) {
            return Err(invalid("duplicate published release asset"));
        }
        let (_name, _path, hash) = expected
            .iter()
            .find(|(expected, _, _)| expected == name)
            .ok_or_else(|| invalid("unexpected published release asset"))?;
        exact(asset, "state", "uploaded")?;
        exact(asset, "digest", &format!("sha256:{hash}"))?;
    }
    Ok(expected
        .iter()
        .filter(|(name, _, _)| !names.contains(name.as_str()))
        .map(|(_, path, _)| path.clone())
        .collect())
}

fn resume_publication(
    github: &impl Github,
    tag: &str,
    verified: &ReviewedDistribution<'_>,
    release: &Value,
) -> Result<(), OrchestrationError> {
    exact(release, "tag_name", tag)?;
    let expected = verified.files()?;
    let missing = missing_assets(release, &expected)?;
    match release.get("draft").and_then(Value::as_bool) {
        Some(false) if missing.is_empty() => Ok(()),
        Some(false) => Err(invalid(
            "published immutable release is missing original assets",
        )),
        Some(true) => {
            if !missing.is_empty() {
                let mut arguments = vec![
                    "release".into(),
                    "upload".into(),
                    tag.into(),
                    "--repo".into(),
                    REPOSITORY.into(),
                ];
                arguments.extend(missing.into_iter().map(std::path::PathBuf::into_os_string));
                verified.unchanged()?;
                github.execute(&arguments)?;
            }
            let uploaded = reload_release(github, tag, release)?;
            if uploaded.get("draft").and_then(Value::as_bool) != Some(true) {
                return Err(invalid("release ceased to be a draft before publication"));
            }
            if !missing_assets(&uploaded, &expected)?.is_empty() {
                return Err(invalid("draft release upload is incomplete"));
            }
            if tag_binding(github, verified.source())?.as_deref() != Some(tag) {
                return Err(invalid(
                    "draft publication does not match the original protected tag",
                ));
            }
            main_ancestry(github, verified.source())?;
            verified.unchanged()?;
            github.execute(&[
                "release".into(),
                "edit".into(),
                tag.into(),
                "--repo".into(),
                REPOSITORY.into(),
                "--draft=false".into(),
            ])?;
            Ok(())
        }
        None => Err(invalid("release draft state is missing")),
    }
}

fn verify_published(
    github: &impl Github,
    tag: &str,
    verified: &ReviewedDistribution<'_>,
) -> Result<(), OrchestrationError> {
    let release = github.api(&format!("releases/tags/{tag}"))?;
    exact(&release, "tag_name", tag)?;
    if release.get("draft").and_then(Value::as_bool) != Some(false)
        || !missing_assets(&release, &verified.files()?)?.is_empty()
    {
        return Err(invalid(
            "release publication did not preserve the complete original distribution",
        ));
    }
    if tag_binding(github, verified.source())?.as_deref() != Some(tag) {
        return Err(invalid(
            "published release does not match the original protected tag",
        ));
    }
    Ok(())
}

fn unix_now() -> Result<u64, OrchestrationError> {
    Ok(raw::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_error| invalid("system time precedes the Unix epoch"))?
        .as_secs())
}

pub fn run(root: &Path, words: &[&str]) -> Result<(), OrchestrationError> {
    if environment("GITHUB_REPOSITORY")? != REPOSITORY {
        return Err(invalid("release queue repository differs"));
    }
    match words {
        ["policy"] => policy(&NativeGithub, &environment("VERSION")?),
        ["handoff"] => handoff(root),
        ["discover"] => {
            if environment("GITHUB_REF")? != "refs/heads/main" {
                return Err(invalid("finalizer must execute from trusted main"));
            }
            let plan = discover(
                &NativeGithub,
                Discovery {
                    event: &environment("GITHUB_EVENT_NAME")?,
                    requested: &environment("QUEUE_RUN_ID")?,
                    trigger: &environment("QUEUE_TRIGGER_RUN_ID")?,
                },
                unix_now()?,
            )?;
            let include = list(&plan, "include")?;
            output(
                "has_pending",
                if include.is_empty() { "false" } else { "true" },
            )?;
            output("matrix", &json!({"include":include}).to_string())?;
            let summary = environment("GITHUB_STEP_SUMMARY")?;
            crate::raw::append(Path::new(&summary), format!("Pending notarization runs: {}.\nExpired run IDs: {}.\nThe discovery scan is limited to the latest {SCAN_LIMIT} original runs.\n", include.len(), plan.get("expiredRunIds").ok_or_else(|| invalid("expiry summary is missing"))?).as_bytes())?;
            Ok(())
        }
        ["finalize"] => {
            if environment("GITHUB_REF")? != "refs/heads/main" {
                return Err(invalid("finalizer must execute from trusted main"));
            }
            finalize(root, &NativeGithub, unix_now()?)
        }
        ["publish"] => {
            if environment("GITHUB_REF")? != "refs/heads/main" {
                return Err(invalid("publisher must execute from trusted main"));
            }
            publish_ready(root, &NativeGithub, unix_now()?)
        }
        _ => Err(invalid(
            "usage: release queue policy|handoff|discover|finalize|publish",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::{
        BUILD_WORKFLOW, Discovery, FINALIZE_WORKFLOW, Github, OrchestrationError, Origin,
        PREDICATE, REPOSITORY, Source, SourceClaims, Value, VerifiedHandoff, active_age,
        bypass_count, bypass_query, completed, create_release, discover, finish, invalid, json,
        list, main_ancestry, missing_assets, original_run, pending_artifact, policy, positive,
        protected_tag, protects_reference, provenance, release_assets, release_for_tag,
        reload_release, tag_binding, timestamp, validate_origin,
    };
    use super::{
        ReadyArtifact, ReviewedDistribution, VerifiedDistribution, provenance_for,
        ready_artifact_binding, saved_ready_run, source_lockfile,
    };
    use std::ffi::OsString;

    #[derive(Default)]
    struct FakeGithub {
        responses: BTreeMap<String, Value>,
        calls: RefCell<Vec<Vec<OsString>>>,
        attested_source: Option<Source>,
    }

    impl FakeGithub {
        fn set(&mut self, path: &str, value: Value) {
            self.responses
                .insert(format!("repos/{REPOSITORY}/{path}"), value);
        }
    }

    impl Github for FakeGithub {
        fn execute(&self, arguments: &[OsString]) -> Result<Vec<u8>, OrchestrationError> {
            self.calls.borrow_mut().push(arguments.to_vec());
            if arguments
                .first()
                .is_some_and(|value| value == "attestation")
            {
                let path = arguments
                    .get(2)
                    .ok_or_else(|| invalid("missing test attestation subject"))?;
                let hash = crate::release_queue::sha256_file_bounded(std::path::Path::new(path))?;
                let mut verified = attestation();
                let digest = verified
                    .pointer_mut("/0/verificationResult/statement/subject/0/digest/sha256")
                    .ok_or_else(|| invalid("missing test attestation digest"))?;
                *digest = json!(hash);
                if let Some(source) = &self.attested_source {
                    let certificate = verified
                        .pointer_mut("/0/verificationResult/signature/certificate")
                        .ok_or_else(|| invalid("missing test certificate"))?;
                    let fields = certificate
                        .as_object_mut()
                        .ok_or_else(|| invalid("test certificate is not an object"))?;
                    fields.insert("sourceRepositoryRef".to_owned(), json!(source.source_ref()));
                    fields.insert(
                        "buildSignerURI".to_owned(),
                        json!(format!(
                            "https://github.com/{REPOSITORY}/{BUILD_WORKFLOW}@{}",
                            source.source_ref()
                        )),
                    );
                }
                return Ok(verified.to_string().into_bytes());
            }
            if arguments.first().is_some_and(|value| value == "release") {
                return Ok(Vec::new());
            }
            let key = arguments
                .last()
                .and_then(|value| value.to_str())
                .ok_or_else(|| invalid("test command has no endpoint"))?;
            self.responses
                .get(key)
                .map(|value| value.to_string().into_bytes())
                .ok_or_else(|| invalid(&format!("unexpected test endpoint {key}")))
        }
    }

    fn source(event: &str) -> Source {
        Source::new(SourceClaims {
            source_sha: "a".repeat(40),
            origin_run_id: 42,
            run_attempt: 2,
            source_ref: if event == "push" {
                "refs/tags/v1.2.3"
            } else {
                "refs/heads/main"
            }
            .to_owned(),
            event: event.to_owned(),
            version: "1.2.3".to_owned(),
        })
        .unwrap()
    }

    fn run() -> Value {
        json!({"id":42,"workflow_id":11,"run_attempt":2,"repository":{"full_name":REPOSITORY},"head_repository":{"full_name":REPOSITORY},
            "path":BUILD_WORKFLOW,"status":"completed","conclusion":"success","event":"workflow_dispatch","head_branch":"main",
            "head_sha":"a".repeat(40),"created_at":"2026-10-01T00:00:00Z"})
    }

    fn set(value: &mut Value, name: &str, replacement: Value) {
        value
            .as_object_mut()
            .unwrap()
            .insert(name.to_owned(), replacement);
    }

    fn artifact() -> Value {
        json!({"id":88,"name":"dist-pending","expired":false,"digest":format!("sha256:{}","b".repeat(64)),"workflow_run":{"id":42,"head_sha":"a".repeat(40)}})
    }

    fn fake() -> FakeGithub {
        let mut github = FakeGithub::default();
        github.set(
            "actions/workflows/release.yml",
            json!({"id":11,"path":BUILD_WORKFLOW,"state":"active"}),
        );
        github.set(
            "actions/workflows/release-finalize.yml",
            json!({"id":12,"path":FINALIZE_WORKFLOW,"state":"active"}),
        );
        github.set("actions/runs/42", run());
        github.set("actions/runs/42/attempts/2", run());
        github.set(
            "actions/workflows/11/runs?status=completed&per_page=100",
            json!({"workflow_runs":[run()]}),
        );
        github.set(
            "actions/runs/42/artifacts?per_page=100",
            json!({"total_count":1,"artifacts":[artifact()]}),
        );
        github.set(
            "actions/artifacts?name=dist-verified-42-2&per_page=100",
            json!({"total_count":0,"artifacts":[]}),
        );
        github.set(
            "git/ref/heads/main",
            json!({"ref":"refs/heads/main","object":{"type":"commit","sha":"a".repeat(40)}}),
        );
        github.set(
            &format!("compare/{}...{}", "a".repeat(40), "a".repeat(40)),
            json!({"status":"identical","merge_base_commit":{"sha":"a".repeat(40)}}),
        );
        github
    }

    #[test]
    fn original_sources_must_remain_in_current_main_history() {
        let created = timestamp("2026-10-01T00:00:00Z").unwrap();
        let mut github = fake();
        let endpoint = format!("compare/{}...{}", "a".repeat(40), "a".repeat(40));
        for status in ["identical", "ahead"] {
            github.set(
                &endpoint,
                json!({"status":status,"merge_base_commit":{"sha":"a".repeat(40)}}),
            );
            validate_origin(&github, source("workflow_dispatch"), 88, created).unwrap();
        }
        for comparison in [
            json!({"status":"behind","merge_base_commit":{"sha":"a".repeat(40)}}),
            json!({"status":"diverged","merge_base_commit":{"sha":"a".repeat(40)}}),
            json!({"status":"changed","merge_base_commit":{"sha":"a".repeat(40)}}),
            json!({"status":"ahead","merge_base_commit":{"sha":"b".repeat(40)}}),
            json!({"status":"ahead"}),
        ] {
            github.set(&endpoint, comparison);
            validate_origin(&github, source("workflow_dispatch"), 88, created).unwrap_err();
        }
        github
            .responses
            .remove(&format!("repos/{REPOSITORY}/git/ref/heads/main"));
        main_ancestry(&github, &source("push")).unwrap_err();
        for reference in [
            json!({"ref":"refs/heads/feature","object":{"type":"commit","sha":"a".repeat(40)}}),
            json!({"ref":"refs/heads/main","object":{"type":"tag","sha":"a".repeat(40)}}),
            json!({"ref":"refs/heads/main","object":{"type":"commit","sha":"invalid/main"}}),
        ] {
            github.set("git/ref/heads/main", reference);
            main_ancestry(&github, &source("push")).unwrap_err();
        }
    }

    #[test]
    fn ids_and_utc_deadlines_reject_ambiguous_inputs() {
        assert_eq!(positive("42").unwrap(), 42);
        for value in [
            "",
            "0",
            "01",
            "-2",
            "2\n",
            "\u{0662}",
            "18446744073709551616",
        ] {
            positive(value).unwrap_err();
        }
        assert_eq!(timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(timestamp("2000-03-01T00:00:00Z").unwrap(), 951_868_800);
        for value in [
            "2026-02-29T00:00:00Z",
            "2026-10-01T24:00:00Z",
            "2026-10-01T00:00:60Z",
            "2026-10-01T00:00:00+00:00",
            "2026-10-01T00:00:00.000Z",
        ] {
            timestamp(value).unwrap_err();
        }
        let created = timestamp("2026-10-01T00:00:00Z").unwrap();
        assert!(active_age(&run(), created.saturating_add(604_799)).unwrap());
        assert!(!active_age(&run(), created.saturating_add(604_800)).unwrap());
        active_age(&run(), created.saturating_sub(1)).unwrap_err();
    }

    #[test]
    fn only_successful_same_repository_main_or_tag_origins_are_eligible() {
        original_run(&run(), 11).unwrap();
        for (name, replacement) in [
            ("event", json!("pull_request")),
            ("head_branch", json!("feature")),
            ("conclusion", json!("failure")),
            ("path", json!(FINALIZE_WORKFLOW)),
            ("workflow_id", json!(99)),
            ("head_repository", json!({"full_name":"fork/domyjob"})),
        ] {
            let mut changed = run();
            set(&mut changed, name, replacement);
            original_run(&changed, 11).unwrap_err();
        }
        let mut tagged = run();
        set(&mut tagged, "event", json!("push"));
        set(&mut tagged, "head_branch", json!("v1.2.3"));
        original_run(&tagged, 11).unwrap();
    }

    #[test]
    fn discovery_is_bounded_and_expiry_is_visible_without_resubmission() {
        let github = fake();
        let created = timestamp("2026-10-01T00:00:00Z").unwrap();
        let plan = discover(
            &github,
            Discovery {
                event: "schedule",
                requested: "",
                trigger: "",
            },
            created,
        )
        .unwrap();
        assert_eq!(
            list(&plan, "include").unwrap(),
            [json!({"run_id":"42","run_attempt":"2","artifact_id":"88"})]
        );
        let expired = discover(
            &github,
            Discovery {
                event: "schedule",
                requested: "",
                trigger: "",
            },
            created.saturating_add(604_800),
        )
        .unwrap();
        assert_eq!(list(&expired, "include").unwrap(), Vec::<Value>::new());
        assert_eq!(list(&expired, "expiredRunIds").unwrap(), [json!(42)]);
        discover(
            &github,
            Discovery {
                event: "workflow_dispatch",
                requested: "42",
                trigger: "",
            },
            created.saturating_add(604_800),
        )
        .unwrap_err();
        discover(
            &github,
            Discovery {
                event: "pull_request",
                requested: "42",
                trigger: "",
            },
            created,
        )
        .unwrap_err();
        discover(
            &github,
            Discovery {
                event: "schedule",
                requested: "42",
                trigger: "",
            },
            created,
        )
        .unwrap_err();
        assert!(
            github
                .calls
                .borrow()
                .iter()
                .all(|arguments| arguments.first().is_some_and(|value| value == "api"))
        );
    }

    #[test]
    fn artifact_identity_and_exact_original_attempt_are_required() {
        let created = timestamp("2026-10-01T00:00:00Z").unwrap();
        validate_origin(&fake(), source("workflow_dispatch"), 88, created).unwrap();
        validate_origin(&fake(), source("workflow_dispatch"), 89, created).unwrap_err();
        for (name, replacement) in [
            ("run_attempt", json!(3)),
            ("head_sha", json!("c".repeat(40))),
            ("id", json!(43)),
        ] {
            let mut github = fake();
            let mut changed = run();
            set(&mut changed, name, replacement);
            github.set("actions/runs/42/attempts/2", changed);
            validate_origin(&github, source("workflow_dispatch"), 88, created).unwrap_err();
        }
        for replacement in [
            json!({"total_count":2,"artifacts":[artifact(),artifact()]}),
            json!({"total_count":101,"artifacts":[]}),
        ] {
            let mut github = fake();
            github.set("actions/runs/42/artifacts?per_page=100", replacement);
            pending_artifact(&github, &run(), true).unwrap_err();
        }
    }

    #[test]
    fn completion_requires_a_successful_trusted_finalizer() {
        let mut github = fake();
        github.set("actions/artifacts?name=dist-verified-42-2&per_page=100", json!({"total_count":1,"artifacts":[{"name":"dist-verified-42-2","expired":false,"workflow_run":{"id":99}}]}));
        let mut marker = run();
        for (name, value) in [
            ("id", json!(99)),
            ("workflow_id", json!(12)),
            ("path", json!(FINALIZE_WORKFLOW)),
            ("event", json!("schedule")),
        ] {
            set(&mut marker, name, value);
        }
        for conclusion in ["failure", "cancelled", "success"] {
            set(&mut marker, "conclusion", json!(conclusion));
            github.set("actions/runs/99", marker.clone());
            assert_eq!(
                completed(&github, 42, 2, 12).unwrap(),
                conclusion == "success"
            );
        }
        for (name, value) in [
            ("id", json!(100)),
            ("head_branch", json!("feature")),
            ("workflow_id", json!(11)),
            ("event", json!("pull_request")),
            ("head_repository", json!({"full_name":"other/fork"})),
        ] {
            let mut untrusted = marker.clone();
            set(&mut untrusted, name, value);
            github.set("actions/runs/99", untrusted);
            assert!(!completed(&github, 42, 2, 12).unwrap());
        }
        set(&mut marker, "id", json!(98));
        github.set("actions/runs/98", marker);
        github.set(
            "actions/artifacts?name=dist-verified-42-2&per_page=100",
            json!({"total_count":101,"artifacts":[
                {"name":"dist-verified-42-2","expired":false,"workflow_run":{"id":99}},
                {"name":"dist-verified-42-2","expired":false,"workflow_run":{"id":98}}
            ]}),
        );
        assert!(completed(&github, 42, 2, 12).unwrap());
        github
            .responses
            .remove(&format!("repos/{REPOSITORY}/actions/runs/99"));
        completed(&github, 42, 2, 12).unwrap_err();
    }

    fn attestation() -> Value {
        json!([{"verificationResult":{"signature":{"certificate":{
            "issuer":"https://token.actions.githubusercontent.com","sourceRepositoryURI":format!("https://github.com/{REPOSITORY}"),
            "sourceRepositoryDigest":"a".repeat(40),"sourceRepositoryRef":"refs/heads/main",
            "buildSignerURI":format!("https://github.com/{REPOSITORY}/{BUILD_WORKFLOW}@refs/heads/main"),"buildSignerDigest":"a".repeat(40),
            "runnerEnvironment":"github-hosted","runInvocationURI":format!("https://github.com/{REPOSITORY}/actions/runs/42/attempts/2")}},
            "statement":{"predicateType":PREDICATE,"subject":[{"digest":{"sha256":"b".repeat(64)}}]},"verifiedTimestamps":[{"time":"2026-10-01T00:00:00Z"}]}}])
    }

    #[test]
    fn provenance_cannot_substitute_a_source_workflow_attempt_or_subject() {
        provenance(
            &attestation(),
            &source("workflow_dispatch"),
            &"b".repeat(64),
        )
        .unwrap();
        for name in [
            "sourceRepositoryDigest",
            "sourceRepositoryRef",
            "buildSignerDigest",
            "buildSignerURI",
            "runInvocationURI",
            "runnerEnvironment",
            "issuer",
        ] {
            let mut changed = attestation();
            let certificate = changed
                .pointer_mut("/0/verificationResult/signature/certificate")
                .unwrap();
            set(certificate, name, json!("forged"));
            provenance(&changed, &source("workflow_dispatch"), &"b".repeat(64)).unwrap_err();
        }
        provenance(
            &attestation(),
            &source("workflow_dispatch"),
            &"c".repeat(64),
        )
        .unwrap_err();
        let mut changed = attestation();
        *changed
            .pointer_mut("/0/verificationResult/verifiedTimestamps")
            .unwrap() = json!([]);
        provenance(&changed, &source("workflow_dispatch"), &"b".repeat(64)).unwrap_err();
    }

    fn protected() -> Value {
        json!({"id":7,"node_id":"ruleset7","target":"tag","enforcement":"active","source_type":"Repository","source":REPOSITORY,"bypass_actors":[],
            "conditions":{"ref_name":{"include":["refs/tags/v*"],"exclude":[]}},"rules":[{"type":"deletion"},{"type":"update"}]})
    }

    fn graphql_protected() -> Value {
        json!({"data":{"repository":{"nameWithOwner":REPOSITORY,"ruleset":{"id":"ruleset7","databaseId":7,
            "target":"TAG","enforcement":"ACTIVE","source":{"__typename":"Repository","nameWithOwner":REPOSITORY},
            "bypassActors":{"totalCount":0,"nodes":[],"pageInfo":{"hasNextPage":false}}}}}})
    }

    fn protect_tags(github: &mut FakeGithub) {
        github.set(
            "rulesets?includes_parents=false&per_page=100",
            json!([{"id":7,"target":"tag","enforcement":"active"}]),
        );
        github.set("rulesets/7", protected());
        github
            .responses
            .insert(format!("query={}", bypass_query(7)), graphql_protected());
    }

    fn limited_tag_policy() -> (FakeGithub, Value) {
        let mut github = fake();
        protect_tags(&mut github);
        let mut limited_rest = protected();
        limited_rest
            .as_object_mut()
            .unwrap()
            .remove("bypass_actors");
        github.set("rulesets/7", limited_rest.clone());
        (github, limited_rest)
    }

    #[test]
    fn limited_token_policy_requires_explicit_consistent_graphql_bypass_metadata() {
        let (mut github, limited_rest) = limited_tag_policy();
        policy(&github, "1.2.3").unwrap();
        assert_eq!(
            bypass_count(&graphql_protected(), &limited_rest).unwrap(),
            0
        );
        assert!(
            github
                .calls
                .borrow()
                .iter()
                .all(|call| call.first().is_some_and(|word| word == "api"))
        );
        assert!(
            !github
                .calls
                .borrow()
                .iter()
                .any(|call| call.iter().any(|word| word
                    .to_str()
                    .is_some_and(|word| word.contains("git/ref") || word.contains("releases"))))
        );
        github
            .responses
            .remove(&format!("query={}", bypass_query(7)));
        protected_tag(&github, "refs/tags/v1.2.3").unwrap_err();
        for version in ["01.2.3", "1.2", "../../outside", "1.2.3\n"] {
            let calls = github.calls.borrow().len();
            policy(&github, version).unwrap_err();
            assert_eq!(github.calls.borrow().len(), calls);
        }
        let calls = github.calls.borrow().len();
        policy(&github, &format!("1.2.3+{}", "a".repeat(128))).unwrap_err();
        assert_eq!(github.calls.borrow().len(), calls);
    }

    #[test]
    fn graphql_ruleset_rejects_missing_mismatched_or_contradictory_metadata() {
        for (pointer, value) in [
            ("/data/repository", Value::Null),
            ("/data/repository/nameWithOwner", json!("other/repository")),
            ("/data/repository/ruleset", Value::Null),
            ("/data/repository/ruleset/databaseId", json!(8)),
            ("/data/repository/ruleset/id", json!("different")),
            ("/data/repository/ruleset/target", json!("BRANCH")),
            ("/data/repository/ruleset/enforcement", json!("EVALUATE")),
            (
                "/data/repository/ruleset/source/__typename",
                json!("Organization"),
            ),
            (
                "/data/repository/ruleset/source/nameWithOwner",
                json!("other/repository"),
            ),
            ("/data/repository/ruleset/bypassActors", Value::Null),
            (
                "/data/repository/ruleset/bypassActors/totalCount",
                json!(-1),
            ),
            (
                "/data/repository/ruleset/bypassActors/totalCount",
                json!(2_147_483_648_u64),
            ),
            (
                "/data/repository/ruleset/bypassActors/totalCount",
                Value::Null,
            ),
            (
                "/data/repository/ruleset/bypassActors/nodes",
                json!([{"id":"actor"}]),
            ),
            (
                "/data/repository/ruleset/bypassActors/pageInfo/hasNextPage",
                json!(true),
            ),
        ] {
            let mut changed = graphql_protected();
            *changed.pointer_mut(pointer).unwrap() = value;
            bypass_count(&changed, &protected()).unwrap_err();
        }
        let mut errors = graphql_protected();
        set(&mut errors, "errors", json!([{"message":"not accessible"}]));
        bypass_count(&errors, &protected()).unwrap_err();
        set(&mut errors, "errors", Value::Null);
        bypass_count(&errors, &protected()).unwrap_err();
    }

    #[test]
    fn nonempty_graphql_bypass_connections_cannot_qualify_a_protected_tag() {
        let (mut github, limited_rest) = limited_tag_policy();
        for count in [1, 2] {
            let mut response = graphql_protected();
            *response
                .pointer_mut("/data/repository/ruleset/bypassActors")
                .unwrap() =
                json!({"totalCount":count,"nodes":[null],"pageInfo":{"hasNextPage":count > 1}});
            assert_eq!(bypass_count(&response, &limited_rest).unwrap(), count);
            bypass_count(&response, &protected()).unwrap_err();
            github
                .responses
                .insert(format!("query={}", bypass_query(7)), response);
            protected_tag(&github, "refs/tags/v1.2.3").unwrap_err();
        }
    }

    #[test]
    fn manual_origins_never_publish_and_tag_origins_require_current_immutable_binding() {
        let mut github = fake();
        assert_eq!(
            tag_binding(&github, &source("workflow_dispatch")).unwrap(),
            None
        );
        assert!(github.calls.borrow().is_empty());
        github.set(
            "git/ref/tags/v1.2.3",
            json!({"ref":"refs/tags/v1.2.3","object":{"type":"tag","sha":"d".repeat(40)}}),
        );
        github.set(
            &format!("git/tags/{}", "d".repeat(40)),
            json!({"object":{"type":"commit","sha":"a".repeat(40)}}),
        );
        protect_tags(&mut github);
        assert_eq!(
            tag_binding(&github, &source("push")).unwrap(),
            Some("v1.2.3".to_owned())
        );
        github.set(
            &format!("git/tags/{}", "d".repeat(40)),
            json!({"object":{"type":"commit","sha":"e".repeat(40)}}),
        );
        tag_binding(&github, &source("push")).unwrap_err();
        for (name, replacement) in [
            ("bypass_actors", json!([{"actor_type":"RepositoryRole"}])),
            ("rules", json!([{"type":"deletion"}])),
            ("enforcement", json!("evaluate")),
            (
                "conditions",
                json!({"ref_name":{"include":["refs/tags/other*"],"exclude":[]}}),
            ),
        ] {
            let mut changed = protected();
            set(&mut changed, name, replacement);
            assert!(!protects_reference(&changed, "refs/tags/v1.2.3").unwrap());
        }
    }

    #[test]
    fn release_asset_retries_refuse_replacements_and_unknown_assets() {
        let expected = vec![(
            "archive.tar.gz".to_owned(),
            std::path::PathBuf::from("archive.tar.gz"),
            "b".repeat(64),
        )];
        assert_eq!(
            missing_assets(&json!({"assets":[]}), &expected).unwrap(),
            [std::path::PathBuf::from("archive.tar.gz")]
        );
        let asset = json!({"name":"archive.tar.gz","state":"uploaded","digest":format!("sha256:{}","b".repeat(64))});
        assert_eq!(
            missing_assets(&json!({"assets":[asset]}), &expected).unwrap(),
            Vec::<std::path::PathBuf>::new()
        );
        missing_assets(&json!({"assets":[asset,asset]}), &expected).unwrap_err();
        for (name, value) in [
            ("digest", json!("sha256:wrong")),
            ("name", json!("unreviewed.exe")),
            ("state", json!("starter")),
        ] {
            let mut changed = asset.clone();
            set(&mut changed, name, value);
            missing_assets(&json!({"assets":[changed]}), &expected).unwrap_err();
        }
    }

    fn tagged_handoff_fixture() -> (tempfile::TempDir, crate::release_queue::Handoff) {
        let (directory, manual) = crate::release_queue::handoff_fixture();
        let source = source("push");
        let manifest_path = manual.dir().join("manifest.json");
        let mut manifest: Value = domyjob_core::ingress::json(
            crate::raw::read_to_string(&manifest_path)
                .unwrap()
                .as_bytes(),
            super::LIMIT,
        )
        .unwrap();
        set(&mut manifest, "source", json!(source));
        for receipt in manifest
            .get_mut("notarizations")
            .unwrap()
            .as_array_mut()
            .unwrap()
        {
            set(receipt, "source", json!(source));
            let target = receipt.get("target").unwrap().as_str().unwrap();
            crate::raw::write(
                &manual
                    .dir()
                    .join("notarization")
                    .join(format!("{target}.json")),
                receipt.to_string().as_bytes(),
            )
            .unwrap();
        }
        crate::raw::write(&manifest_path, manifest.to_string().as_bytes()).unwrap();
        let handoff = crate::release_queue::Handoff::inspect(manual.dir(), &source).unwrap();
        (directory, handoff)
    }

    fn publication_fake(handoff: &crate::release_queue::Handoff) -> FakeGithub {
        let mut github = fake();
        github.attested_source = Some(source("push"));
        github.set(
            "git/ref/tags/v1.2.3",
            json!({"ref":"refs/tags/v1.2.3","object":{"type":"commit","sha":"a".repeat(40)}}),
        );
        protect_tags(&mut github);
        let assets: Vec<_> = release_assets(handoff)
            .unwrap()
            .iter()
            .map(|(name, _path, hash)| json!({"name":name,"state":"uploaded","digest":format!("sha256:{hash}")}))
            .collect();
        let draft = json!({"id":123,"tag_name":"v1.2.3","draft":true,"assets":assets});
        github.set("releases?per_page=100", json!([draft]));
        github.set("releases/123", draft);
        github
    }

    fn reviewed_fixture<'a>(verified: &'a VerifiedDistribution<'a>) -> ReviewedDistribution<'a> {
        let lock = b"version = 4\n";
        ReviewedDistribution::bind(
            verified,
            crate::dependencies::audited_lockfile_fixture(lock),
            lock,
        )
        .unwrap()
    }

    #[test]
    fn publication_audit_fetches_the_original_lockfile_and_rejects_substitution() {
        let original = source("push");
        let endpoint = format!("contents/Cargo.lock?ref={}", original.source_sha());
        let lock = b"version = 4\n";
        let metadata = json!({
            "name":"Cargo.lock", "path":"Cargo.lock", "type":"file", "encoding":"base64",
            "size":lock.len(), "content":data_encoding::BASE64.encode(lock)
        });
        let mut github = fake();
        github.set(&endpoint, metadata.clone());
        assert_eq!(source_lockfile(&github, &original).unwrap(), lock);
        for (name, value) in [
            ("path", json!("other/Cargo.lock")),
            ("type", json!("symlink")),
            ("encoding", json!("none")),
            ("size", json!(0)),
            ("size", json!(lock.len() + 1)),
            ("size", json!(1_048_577)),
            ("content", json!("not base64")),
        ] {
            let mut changed = metadata.clone();
            set(&mut changed, name, value);
            github.set(&endpoint, changed);
            source_lockfile(&github, &original).unwrap_err();
        }
        let (_directory, handoff) = tagged_handoff_fixture();
        let verified = VerifiedHandoff::new(&publication_fake(&handoff), &handoff).unwrap();
        let distribution = VerifiedDistribution::Legacy(&verified);
        ReviewedDistribution::bind(
            &distribution,
            crate::dependencies::audited_lockfile_fixture(lock),
            b"another lockfile snapshot",
        )
        .unwrap_err();
    }

    #[test]
    fn draft_release_reloads_bind_ids_and_refuse_ambiguous_inventory() {
        let mut github = fake();
        let draft = json!({"id":123,"tag_name":"v1.2.3","draft":true,"assets":[]});
        github.set("releases?per_page=100", json!([draft]));
        github.set("releases/123", draft.clone());
        assert_eq!(
            release_for_tag(&github, "v1.2.3").unwrap(),
            Some(draft.clone())
        );
        assert!(!github.calls.borrow().iter().any(|call| {
            call.last()
                .is_some_and(|value| value.to_string_lossy().contains("releases/tags/"))
        }));
        github.set("releases?per_page=100", json!([draft, draft]));
        release_for_tag(&github, "v1.2.3").unwrap_err();
        for (name, value) in [("id", json!(124)), ("tag_name", json!("v9.9.9"))] {
            let mut changed = draft.clone();
            set(&mut changed, name, value);
            github.set("releases/123", changed);
            reload_release(&github, "v1.2.3", &draft).unwrap_err();
        }
    }

    #[test]
    fn new_release_stays_draft_until_uploaded_bytes_and_current_main_are_verified() {
        let (_directory, handoff) = tagged_handoff_fixture();
        let mut github = publication_fake(&handoff);
        let origin = Origin {
            source: source("push"),
            artifact_id: 88,
        };
        let verified = VerifiedHandoff::new(&github, &handoff).unwrap();
        github.calls.borrow_mut().clear();
        create_release(
            &github,
            "v1.2.3",
            &origin,
            &reviewed_fixture(&VerifiedDistribution::Legacy(&verified)),
        )
        .unwrap();
        let calls = github.calls.borrow();
        let create = calls.first().unwrap();
        assert_eq!(create.get(1).unwrap(), "create");
        assert!(create.iter().any(|argument| argument == "--draft"));
        assert!(create.iter().any(|argument| argument == "--verify-tag"));
        let publish = calls.last().unwrap();
        assert_eq!(publish.get(1).unwrap(), "edit");
        assert!(publish.iter().any(|argument| argument == "--draft=false"));
        let comparison = format!(
            "repos/{REPOSITORY}/compare/{}...{}",
            "a".repeat(40),
            "a".repeat(40)
        );
        assert_eq!(
            calls
                .get(calls.len().saturating_sub(2))
                .unwrap()
                .last()
                .unwrap(),
            comparison.as_str()
        );
        assert!(
            calls
                .get(1)
                .unwrap()
                .last()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("releases?per_page=100")
        );
        drop(calls);
        let endpoint = format!("compare/{}...{}", "a".repeat(40), "a".repeat(40));
        github.set(
            &endpoint,
            json!({"status":"diverged","merge_base_commit":{"sha":"b".repeat(40)}}),
        );
        assert_create_stays_draft(&github, &origin, &verified);
    }

    fn assert_create_stays_draft(
        github: &FakeGithub,
        origin: &Origin,
        verified: &VerifiedHandoff<'_>,
    ) {
        github.calls.borrow_mut().clear();
        create_release(
            github,
            "v1.2.3",
            origin,
            &reviewed_fixture(&VerifiedDistribution::Legacy(verified)),
        )
        .unwrap_err();
        assert!(
            github
                .calls
                .borrow()
                .iter()
                .all(|arguments| !arguments.iter().any(|argument| argument == "--draft=false"))
        );
    }

    #[test]
    fn new_draft_asset_mismatch_or_early_publication_cannot_reach_publish() {
        let (_directory, handoff) = tagged_handoff_fixture();
        let mut github = publication_fake(&handoff);
        let verified = VerifiedHandoff::new(&github, &handoff).unwrap();
        let origin = Origin {
            source: source("push"),
            artifact_id: 88,
        };
        for draft in [
            json!({"id":123,"tag_name":"v1.2.3","draft":false,"assets":[]}),
            json!({"id":123,"tag_name":"v1.2.3","draft":true,"assets":[{"name":"unreviewed.exe","state":"uploaded","digest":format!("sha256:{}","b".repeat(64))}]}),
        ] {
            github.set("releases?per_page=100", json!([draft]));
            github.set("releases/123", draft);
            assert_create_stays_draft(&github, &origin, &verified);
        }
    }

    #[test]
    fn pending_status_never_calls_publication_or_creates_a_completion() {
        let called = std::cell::Cell::new(false);
        let accepted = finish(crate::release::NotaryOutcome::Pending, |_token| {
            called.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!accepted);
        assert!(!called.get());
    }

    #[test]
    fn package_handoff_cannot_use_the_legacy_publication_path() {
        let (_directory, handoff) = crate::release_queue::package_handoff_fixture();
        let verified = VerifiedHandoff::new(&fake(), &handoff).unwrap();
        VerifiedDistribution::Legacy(&verified)
            .unchanged()
            .unwrap_err();
    }

    #[test]
    fn finalizer_attestation_cannot_be_substituted_for_original_build_provenance() {
        let mut entry = attestation();
        let signer = entry
            .pointer_mut("/0/verificationResult/signature/certificate/buildSignerURI")
            .unwrap();
        *signer = json!(format!(
            "https://github.com/{REPOSITORY}/{FINALIZE_WORKFLOW}@refs/heads/main"
        ));
        let original = source("workflow_dispatch");
        provenance(&entry, &original, &"b".repeat(64)).unwrap_err();
        provenance_for(&entry, &original, &"b".repeat(64), FINALIZE_WORKFLOW).unwrap();
        provenance_for(
            &attestation(),
            &original,
            &"b".repeat(64),
            FINALIZE_WORKFLOW,
        )
        .unwrap_err();
    }

    fn ready_artifact() -> Value {
        json!({"id":500,"name":"dist-ready-42-2","expired":false,"digest":format!("sha256:{}","d".repeat(64)),"workflow_run":{"id":99}})
    }

    #[test]
    fn saved_ready_name_is_bound_to_one_exact_live_artifact_before_reuse() {
        let mut github = fake();
        let selected = ReadyArtifact {
            run_id: 99,
            artifact_id: 500,
            digest: format!("sha256:{}", "d".repeat(64)),
        };
        let path = "actions/runs/99/artifacts?per_page=100";
        github.set(
            path,
            json!({"total_count":1,"artifacts":[ready_artifact()]}),
        );
        ready_artifact_binding(&github, &source("workflow_dispatch"), &selected).unwrap();
        for (key, value) in [
            ("id", json!(501)),
            ("digest", json!(format!("sha256:{}", "e".repeat(64)))),
            ("expired", json!(true)),
        ] {
            let mut changed = ready_artifact();
            set(&mut changed, key, value);
            github.set(path, json!({"total_count":1,"artifacts":[changed]}));
            ready_artifact_binding(&github, &source("workflow_dispatch"), &selected).unwrap_err();
        }
        github.set(
            path,
            json!({"total_count":2,"artifacts":[ready_artifact(),ready_artifact()]}),
        );
        ready_artifact_binding(&github, &source("workflow_dispatch"), &selected).unwrap_err();
    }

    #[test]
    fn unrelated_artifact_with_a_ready_name_cannot_block_a_trusted_candidate() {
        let mut github = fake();
        let mut trusted = run();
        for (name, value) in [
            ("id", json!(99)),
            ("workflow_id", json!(12)),
            ("path", json!(FINALIZE_WORKFLOW)),
            ("event", json!("schedule")),
            ("conclusion", json!("failure")),
        ] {
            set(&mut trusted, name, value);
        }
        github.set("actions/runs/99", trusted);
        github.set("actions/runs/100", run());
        let mut unrelated = ready_artifact();
        set(&mut unrelated, "workflow_run", json!({"id":100}));
        github.set(
            "actions/artifacts?name=dist-ready-42-2&per_page=100",
            json!({"total_count":2,"artifacts":[unrelated,ready_artifact()]}),
        );
        let selected = saved_ready_run(&github, &source("workflow_dispatch"))
            .unwrap()
            .unwrap();
        assert_eq!(selected.run_id, 99);
        assert_eq!(selected.artifact_id, 500);
    }

    #[test]
    fn verified_handoff_refuses_changes_after_provenance_or_native_acceptance() {
        let (_directory, handoff) = crate::release_queue::handoff_fixture();
        let github = fake();
        let verified = VerifiedHandoff::new(&github, &handoff).unwrap();
        let manifest = handoff.dir().join("manifest.json");
        let original = crate::raw::read_to_string(&manifest).unwrap();
        crate::raw::write(&manifest, format!("{original}\n").as_bytes()).unwrap();
        verified.unchanged().unwrap_err();
        crate::raw::write(&manifest, original.as_bytes()).unwrap();
        verified.unchanged().unwrap();
        let archive = handoff.archive("aarch64-apple-darwin").unwrap();
        crate::raw::write(archive, b"different signed bytes").unwrap();
        verified.unchanged().unwrap_err();
        assert_eq!(github.calls.borrow().len(), 6);
        assert!(github.calls.borrow().iter().all(|arguments| {
            arguments
                .first()
                .is_some_and(|value| value == "attestation")
                && arguments.iter().any(|value| value == "--source-digest")
                && arguments.iter().any(|value| value == "--signer-digest")
                && arguments
                    .iter()
                    .any(|value| value == "--deny-self-hosted-runners")
        }));
    }
}
