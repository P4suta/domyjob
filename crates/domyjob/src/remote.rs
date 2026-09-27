use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::config::{Binary, Config, ConfigError, Machine};
use crate::dist::Deliverable;
use crate::domain::{BlobId, MachineName, Nonce};
use crate::paths::{Dirs, Family};
use crate::protocol::{Frame, Hello, Refusal, Reply, Request, VERSION, build_key, wire};
use crate::snapshot::{Origin, SnapshotError};
use crate::template::{Arg, Argv, Bindings, TemplateError};

fn witness_path(dirs: &Dirs, machine: &MachineName) -> PathBuf {
    let tag = blake3::hash(machine.as_str().as_bytes()).to_hex();
    dirs.state
        .join("witness")
        .join(format!("{}.json", tag.get(..32).unwrap_or(tag.as_str())))
}

fn save_witness(
    path: &std::path::Path,
    head: &crate::audit::Head,
) -> Result<(), crate::state_file::StateError> {
    crate::state_file::write_json(path, head)
}

impl RemoteError {
    #[must_use]
    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Empty { machine }
            | Self::Start { machine, .. }
            | Self::Pipe { machine, .. }
            | Self::Exited { machine, .. }
            | Self::Garbled { machine, .. }
            | Self::Refused { machine, .. }
            | Self::Unreadable { machine, .. }
            | Self::Silent { machine, .. }
            | Self::Stream { machine, .. }
            | Self::Unexpected { machine, .. }
            | Self::Protocol { machine, .. }
            | Self::Newer { machine, .. }
            | Self::Probe { machine, .. }
            | Self::Tampered { machine, .. }
            | Self::BuildMismatch { machine, .. }
            | Self::AuditRolledBack { machine, .. }
            | Self::AuditRewritten { machine, .. }
            | Self::Outdated { machine, .. }
            | Self::Unbuilt { machine, .. } => Some(machine),
            Self::TransferId(_)
            | Self::Config(_)
            | Self::Template { .. }
            | Self::Dist(_)
            | Self::State(_)
            | Self::Snapshot(_)
            | Self::Io(_) => None,
        }
    }
}

pub fn witnessed(
    dirs: &Dirs,
    machine: &MachineName,
) -> Result<Option<crate::audit::Head>, RemoteError> {
    Ok(crate::state_file::read_json(&witness_path(dirs, machine))?)
}

pub fn forget_witness(dirs: &Dirs, machine: &MachineName) -> Result<(), RemoteError> {
    Ok(crate::state_file::remove_file(&witness_path(
        dirs, machine,
    ))?)
}

const SAID_LIMIT: u64 = 64 << 10;

fn drain(errors: Option<std::process::ChildStderr>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let Some(mut errors) = errors else {
            return String::new();
        };
        let read = crate::bounded::to_end(
            &mut std::io::Read::take(std::io::Read::by_ref(&mut errors), SAID_LIMIT),
            SAID_LIMIT,
        );
        let rest = std::io::copy(&mut errors, &mut std::io::sink());
        match rest {
            Ok(_) | Err(_) => {}
        }
        match read {
            Ok(kept) => String::from_utf8_lossy(&kept).into_owned(),
            Err(_) => String::new(),
        }
    })
}

fn reported_stderr(
    stderr: &mut dyn std::io::BufRead,
    tell: &(dyn Fn(&str) + Sync),
) -> std::io::Result<String> {
    let mut said = String::new();
    loop {
        let remaining = crate::bounded::CAPTURE.saturating_sub(crate::domain::len_u64(said.len()));
        let line = match crate::bounded::line(stderr, remaining) {
            Ok(line) => line,
            Err(error) => {
                match std::io::copy(stderr, &mut std::io::sink()) {
                    Ok(_) | Err(_) => {}
                }
                return Err(error);
            }
        };
        if line.is_empty() {
            return Ok(said);
        }
        let line = match String::from_utf8(line) {
            Ok(line) => line,
            Err(error) => {
                match std::io::copy(stderr, &mut std::io::sink()) {
                    Ok(_) | Err(_) => {}
                }
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
        };
        let visible = line.trim_end_matches(['\r', '\n']);
        tell(visible);
        said.push_str(visible);
        said.push('\n');
    }
}

fn pipe_error(machine: String, doing: &'static str) -> impl FnOnce(std::io::Error) -> RemoteError {
    move |source| RemoteError::Pipe {
        machine,
        doing,
        source,
    }
}

fn reap_after_failure(child: &mut std::process::Child) {
    match child.kill() {
        Ok(()) | Err(_) => {}
    }
    match child.wait() {
        Ok(_) | Err(_) => {}
    }
}

fn said(errors: &str) -> String {
    let lines: Vec<String> = errors
        .lines()
        .map(crate::terminal::clean)
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty() && !line.starts_with("Control socket connect("))
        .collect();
    let last = lines
        .get(lines.len().saturating_sub(3)..)
        .unwrap_or_default();
    if last.is_empty() {
        String::new()
    } else {
        format!(": {}", last.join("; "))
    }
}

static SHARED: std::sync::Mutex<
    std::collections::BTreeMap<MachineName, Option<std::process::Child>>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

struct PendingShare(MachineName);

impl PendingShare {
    fn claim(machine: &MachineName) -> Result<Option<Self>, RemoteError> {
        let mut shared = shared_lock(machine)?;
        if shared.contains_key(machine) {
            return Ok(None);
        }
        shared.insert(machine.clone(), None);
        drop(shared);
        Ok(Some(Self(machine.clone())))
    }
}

impl Drop for PendingShare {
    fn drop(&mut self) {
        let mut shared = SHARED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(shared.get(&self.0), Some(None)) {
            shared.remove(&self.0);
        }
    }
}

fn shared_lock(
    machine: &MachineName,
) -> Result<
    std::sync::MutexGuard<
        'static,
        std::collections::BTreeMap<MachineName, Option<std::process::Child>>,
    >,
    RemoteError,
> {
    SHARED.lock().map_err(|_poisoned| RemoteError::Pipe {
        machine: machine.to_string(),
        doing: "sharing a connection",
        source: std::io::Error::other("the shared connection registry is poisoned"),
    })
}

static SESSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransferId(Nonce);

impl TransferId {
    fn fresh() -> Result<Self, crate::domain::Invalid> {
        Ok(Self(Nonce::generate()?))
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

fn session() -> &'static str {
    SESSION.get_or_init(|| {
        let mut random = [0u8; 16];
        match getrandom::fill(&mut random) {
            Ok(()) => crate::trust::hex(&random),
            Err(_no_entropy) => format!("{:08x}", std::process::id()),
        }
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("could not generate a unique setup ID: {0}")]
    TransferId(crate::domain::Invalid),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("transport {transport}: {source}")]
    Template {
        transport: String,
        source: TemplateError,
    },
    #[error("{machine}: the transport command is empty")]
    Empty { machine: String },
    #[error("{machine}: {program} could not start: {source}")]
    Start {
        machine: String,
        program: String,
        source: std::io::Error,
    },
    #[error("{machine}: the connection failed while {doing}: {source}")]
    Pipe {
        machine: String,
        doing: &'static str,
        source: std::io::Error,
    },
    #[error("{machine}: exited with {status} while {doing}{said}")]
    Exited {
        machine: String,
        doing: &'static str,
        status: std::process::ExitStatus,
        said: String,
    },
    #[error("{machine}: sent something that is not a reply ({detail}): {line:?}")]
    Garbled {
        machine: String,
        detail: String,
        line: String,
    },
    #[error("{machine}: {}", .refusal.detail)]
    Refused { machine: String, refusal: Refusal },
    #[error("{machine}: job {job} cannot be read ({why}); `domyjob doctor` checks the machine")]
    Unreadable {
        machine: String,
        job: crate::domain::JobId,
        why: crate::terminal::RemoteText,
    },
    #[error(
        "{machine}: went silent while {doing}; nothing arrived for too long, so the connection was given up (the job, if any, keeps running there)"
    )]
    Silent {
        machine: String,
        doing: &'static str,
    },
    #[error("{machine}: {problem}")]
    Stream {
        machine: String,
        doing: &'static str,
        problem: crate::framed::Unframed,
    },
    #[error("{machine}: expected {expected}, got {got:?}")]
    Unexpected {
        machine: String,
        expected: &'static str,
        got: Box<Reply>,
    },
    #[error(
        "{machine}: runs domyjob {version}, whose messages differ from this one's (wire {theirs}, here {ours})",
        ours = wire()
    )]
    Protocol {
        machine: String,
        theirs: crate::terminal::RemoteText,
        version: crate::terminal::RemoteText,
    },
    #[error(
        "{machine}: runs domyjob {version}, newer than this one ({VERSION}); it was left as it is"
    )]
    Newer {
        machine: String,
        version: crate::terminal::RemoteText,
    },
    #[error("{machine}: could not tell which operating system it runs: {detail}")]
    Probe { machine: String, detail: String },
    #[error(transparent)]
    Dist(#[from] crate::dist::DistError),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error(
        "{machine} reports running a binary with sha256 {reported}, not the {expected} that was installed"
    )]
    Tampered {
        machine: String,
        expected: String,
        reported: crate::terminal::RemoteText,
    },
    #[error(
        "{machine} built source stamp {reported}, not the expected {expected}; the staged binary was not installed"
    )]
    BuildMismatch {
        machine: String,
        expected: &'static str,
        reported: crate::terminal::RemoteText,
    },
    #[error(
        "the audit log on {machine} went back from {known} entries to {now}; entries were removed since this machine last saw it (if you reset {machine} yourself, `domyjob machines rewitness {machine}` shows what changed)"
    )]
    AuditRolledBack {
        machine: String,
        known: u64,
        now: u64,
    },
    #[error(
        "the audit log on {machine} no longer matches what this machine saw at entry {seq}; its history was rewritten (if you reset {machine} yourself, `domyjob machines rewitness {machine}` shows what changed)"
    )]
    AuditRewritten { machine: String, seq: u64 },
    #[error(
        "{machine} runs domyjob {version}, whose messages differ from this one's, and no matching copy can be installed there: {cause}"
    )]
    Outdated {
        machine: String,
        version: crate::terminal::RemoteText,
        cause: Box<crate::dist::DistError>,
    },
    #[error(
        "{machine} has no domyjob {VERSION}{was}, and this build has no signed release to fetch one from"
    )]
    Unbuilt { machine: String, was: String },
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    pub hello: Hello,
    pub placement: Placement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Installed,
    Managed,
}

const INSTALLED_UNIX: &str = "for c in \"$(command -v domyjob 2>/dev/null)\" \"$HOME/.local/bin/domyjob\" \
    \"$HOME/.cargo/bin/domyjob\" /opt/homebrew/bin/domyjob /usr/local/bin/domyjob; do \
    [ -n \"$c\" ] && [ -x \"$c\" ] && exec \"$c\" node; done; exit 127";

impl Facts {
    #[must_use]
    pub fn family(&self) -> Family {
        if self.hello.os.as_raw_str() == "windows" {
            Family::Windows
        } else {
            Family::Unix
        }
    }

    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        vec![
            format!("os={}", self.hello.os),
            format!("arch={}", self.hello.arch),
        ]
    }
}

fn facts_path(dirs: &Dirs, machine: &Machine) -> PathBuf {
    dirs.cache
        .join("machines")
        .join(format!("{}.json", machine.name))
}

pub fn cached_facts(dirs: &Dirs, machine: &Machine) -> Result<Option<Facts>, RemoteError> {
    match crate::state_file::read_json::<Facts>(&facts_path(dirs, machine)) {
        Ok(facts) => Ok(facts),
        Err(crate::state_file::StateError::Json { .. }) => Ok(None),
        Err(other) => Err(other.into()),
    }
}

fn save_facts(dirs: &Dirs, machine: &Machine, facts: &Facts) -> Result<(), RemoteError> {
    if matches!(cached_facts(dirs, machine)?, Some(known) if known == *facts) {
        return Ok(());
    }
    Ok(crate::state_file::write_json(
        &facts_path(dirs, machine),
        facts,
    )?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Remote {
    Node(Family, Placement),
    Probe,
    WindowsArch,
    Install(Family, TransferId),
    Staged(Family, TransferId),
    Promote(Family, TransferId),
    Discard(Family, TransferId),
    Uninstall(Family, Placement),
    Scrub(Family),
    Build(Family, TransferId, BlobId),
}

impl Remote {
    fn install(family: Family, transfer: &TransferId) -> Self {
        Self::Install(family, transfer.clone())
    }

    fn build(family: Family, transfer: &TransferId, source: &BlobId) -> Self {
        Self::Build(family, transfer.clone(), source.clone())
    }

    fn staged(family: Family, transfer: &TransferId) -> Self {
        Self::Staged(family, transfer.clone())
    }

    fn promote(family: Family, transfer: &TransferId) -> Self {
        Self::Promote(family, transfer.clone())
    }

    fn discard(family: Family, transfer: &TransferId) -> Self {
        Self::Discard(family, transfer.clone())
    }
}

fn build_unix(transfer: &TransferId, source: &BlobId) -> Arg {
    Arg::concat(&[
        Arg::literal("set -e; transfer='"),
        Arg::word(transfer),
        Arg::joined(&[
            "'; PATH=\"$HOME/.cargo/bin:$HOME/.local/share/mise/shims:$PATH\"; ",
            "c=\"$HOME/.cache/domyjob\"; s=\"$c/source-$$\"; t=\"$c/build/",
            build_key(),
            "/",
        ]),
        Arg::word(source),
        Arg::joined(&[
            "\"; trap 'cd /; rm -rf \"$s\"' EXIT; rm -rf \"$s\"; mkdir -p \"$s\"; ",
            "tar -x -m -C \"$s\"; cd \"$s\"; ",
            "MISE_TRUSTED_CONFIG_PATHS=\"$s\"; export MISE_TRUSTED_CONFIG_PATHS; ",
            "(while :; do sleep 5; printf . >&2; done) & beat=$!; trap 'kill $beat 2>/dev/null; cd /; rm -rf \"$s\"' EXIT; ",
            "DOMYJOB_EXPECTED_BUILD_STAMP=",
            crate::protocol::BUILD_STAMP,
            " cargo build --release --locked -p domyjob --target-dir \"$t\" >&2; ",
            "d=\"$c/bin/",
            build_key(),
            "\"; p=\"$d/domyjob.incoming-$transfer\"; mkdir -p \"$d\"; cp \"$t/release/domyjob\" \"$p.$$\"; ",
            "chmod 755 \"$p.$$\"; mv -f \"$p.$$\" \"$p\"",
        ]),
    ])
}

fn build_windows_script(transfer: &TransferId, source: &BlobId) -> Arg {
    Arg::concat(&[
        Arg::literal("$ErrorActionPreference = 'Stop'; $transfer = '"),
        Arg::word(transfer),
        Arg::joined(&[
            "'; ",
            "$c = Join-Path $env:USERPROFILE '.cache\\domyjob'; $s = Join-Path $c \"source-$PID\"; ",
            "$t = \"$s.tar\"; try { ",
            "if (Test-Path $s) { Remove-Item -Recurse -Force $s }; New-Item -ItemType Directory -Force $s | Out-Null; ",
            "$b = Join-Path $env:TEMP ('domyjob-source-' + $transfer + '.b64'); ",
            "[IO.File]::WriteAllBytes($t, [Convert]::FromBase64String(((Get-Content -Raw $b) -replace '\\s', ''))); Remove-Item -Force $b; ",
            "tar -x -m -f $t -C $s; if ($LASTEXITCODE) { throw \"tar exited with $LASTEXITCODE\" }; ",
            "Set-Location $s; $env:MISE_TRUSTED_CONFIG_PATHS = $s; ",
            "$target = Join-Path $c 'build\\",
            build_key(),
            "\\",
        ]),
        Arg::word(source),
        Arg::joined(&[
            "'; $exe = Join-Path $target 'release\\domyjob.exe'; ",
            "$env:DOMYJOB_EXPECTED_BUILD_STAMP = '",
            crate::protocol::BUILD_STAMP,
            "'; ",
            "$job = Start-Job -ArgumentList $s,$target,$env:DOMYJOB_EXPECTED_BUILD_STAMP -ScriptBlock { param($source,$target,$expected) Set-Location $source; $env:MISE_TRUSTED_CONFIG_PATHS = $source; $env:DOMYJOB_EXPECTED_BUILD_STAMP = $expected; & cargo build --release --locked -p domyjob --target-dir $target; if ($LASTEXITCODE -ne 0) { throw \"cargo build exited with $LASTEXITCODE\" } }; ",
            "while ($job.State -eq 'NotStarted' -or $job.State -eq 'Running') { Wait-Job $job -Timeout 5 | Out-Null; [Console]::Error.Write('.') }; ",
            "Receive-Job $job -ErrorAction Continue | ForEach-Object { [Console]::Error.WriteLine($_) }; ",
            "if ($job.State -ne 'Completed') { throw 'cargo build failed' }; Remove-Job $job; ",
            "if (-not (Test-Path $exe)) { throw 'cargo build produced no domyjob.exe' }; ",
            "$d = Join-Path $c 'bin\\",
            build_key(),
            "'; New-Item -ItemType Directory -Force $d | Out-Null; ",
            "$stage = Join-Path $d ('domyjob.incoming-' + $transfer + '.exe'); Copy-Item -Force $exe $stage; ",
            "} finally { Set-Location $env:USERPROFILE; Remove-Item -LiteralPath $s,$t -Recurse -Force -ErrorAction SilentlyContinue }",
        ]),
    ])
}

fn build_windows(transfer: &TransferId, source: &BlobId) -> Arg {
    build_windows_with(&build_windows_script(transfer, source), transfer)
}

fn build_windows_with(script: &Arg, transfer: &TransferId) -> Arg {
    Arg::concat(&[
        Arg::literal("findstr . > %TEMP%\\domyjob-source-"),
        Arg::word(transfer),
        Arg::literal(
            ".b64 && powershell -NoProfile -NonInteractive -InputFormat None -EncodedCommand ",
        ),
        Arg::powershell_encoded(script),
    ])
}

#[cfg(test)]
pub(crate) fn windows_source_build_for_test(
    cache: &std::path::Path,
    source: &BlobId,
) -> (Arg, TransferId) {
    let transfer = TransferId::fresh().unwrap();
    let original = build_windows_script(&transfer, source).into_string();
    let assignment = "$c = Join-Path $env:USERPROFILE '.cache\\domyjob';";
    assert!(original.contains(assignment));
    let assigned = format!(
        "$c = {};",
        crate::shell::powershell_quote(&cache.display().to_string())
    );
    (
        build_windows_with(
            &Arg::for_test(original.replacen(assignment, &assigned, 1)),
            &transfer,
        ),
        transfer,
    )
}

const PROBE_WINDOWS: &str = "[System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture";

fn install_windows_script(transfer: &TransferId) -> Arg {
    Arg::concat(&[
        Arg::joined(&[
            "mkdir %USERPROFILE%\\.cache\\domyjob\\bin\\",
            build_key(),
            " 2>nul & findstr . > %TEMP%\\domyjob-",
            build_key(),
            "-",
        ]),
        Arg::word(transfer),
        Arg::joined(&[
            ".b64 && certutil -f -decode %TEMP%\\domyjob-",
            build_key(),
            "-",
        ]),
        Arg::word(transfer),
        Arg::joined(&[
            ".b64 %USERPROFILE%\\.cache\\domyjob\\bin\\",
            build_key(),
            "\\domyjob.incoming-",
        ]),
        Arg::word(transfer),
        Arg::joined(&[".exe >nul && del %TEMP%\\domyjob-", build_key(), "-"]),
        Arg::word(transfer),
        Arg::literal(".b64"),
    ])
}

const UNINSTALL_INSTALLED_UNIX: &str = "for c in \"$(command -v domyjob 2>/dev/null)\" \"$HOME/.local/bin/domyjob\" \
    \"$HOME/.cargo/bin/domyjob\" /opt/homebrew/bin/domyjob /usr/local/bin/domyjob; do \
    [ -n \"$c\" ] && [ -x \"$c\" ] && exec \"$c\" self uninstall --yes; done; exit 127";

const SCRUB_WINDOWS: &str =
    "if exist %USERPROFILE%\\.cache\\domyjob rmdir /s /q %USERPROFILE%\\.cache\\domyjob";

fn uninstall_argv(family: Family, placement: Placement) -> Vec<Arg> {
    match (family, placement) {
        (Family::Unix, Placement::Managed) => vec![
            Arg::literal("sh"),
            Arg::literal("-c"),
            Arg::joined(&[
                "exec \"$HOME/.cache/domyjob/bin/",
                build_key(),
                "/domyjob\" self uninstall --yes",
            ]),
        ],
        (Family::Unix, Placement::Installed) => vec![
            Arg::literal("sh"),
            Arg::literal("-c"),
            Arg::literal(UNINSTALL_INSTALLED_UNIX),
        ],
        (Family::Windows, Placement::Managed) => cmd(uninstall_windows()),
        (Family::Windows, Placement::Installed) => {
            cmd(Arg::literal("domyjob self uninstall --yes"))
        }
    }
}

fn scrub_argv(family: Family) -> Vec<Arg> {
    match family {
        Family::Unix => vec![
            Arg::literal("sh"),
            Arg::literal("-c"),
            Arg::literal("rm -rf \"$HOME/.cache/domyjob\""),
        ],
        Family::Windows => cmd(Arg::literal(SCRUB_WINDOWS)),
    }
}

fn uninstall_windows() -> Arg {
    Arg::joined(&[
        ".\\.cache\\domyjob\\bin\\",
        build_key(),
        "\\domyjob.exe self uninstall --yes",
    ])
}

fn staged_windows(transfer: &TransferId) -> Arg {
    Arg::concat(&[
        Arg::joined(&[
            ".\\.cache\\domyjob\\bin\\",
            build_key(),
            "\\domyjob.incoming-",
        ]),
        Arg::word(transfer),
        Arg::literal(".exe node"),
    ])
}

pub(crate) fn promote_windows_script(transfer: &TransferId) -> Arg {
    Arg::concat(&[
        Arg::joined(&[
            "cd /d %USERPROFILE%\\.cache\\domyjob\\bin\\",
            build_key(),
            " && (del /q domyjob.old-*.exe 2>nul & if exist domyjob.exe move /y domyjob.exe domyjob.old-%RANDOM%%RANDOM%.exe >nul) && move /y domyjob.incoming-",
        ]),
        Arg::word(transfer),
        Arg::literal(".exe domyjob.exe >nul"),
    ])
}

fn discard_windows_script(transfer: &TransferId) -> Arg {
    Arg::concat(&[
        Arg::literal("$transfer = '"),
        Arg::word(transfer),
        Arg::joined(&[
            "'; $d = Join-Path $env:USERPROFILE '.cache\\domyjob\\bin\\",
            build_key(),
            "'; $stage = Join-Path $d ('domyjob.incoming-' + $transfer + '.exe'); ",
            "$upload = Join-Path $env:TEMP ('domyjob-",
            build_key(),
            "-' + $transfer + '.b64'); ",
            "$source = Join-Path $env:TEMP ('domyjob-source-' + $transfer + '.b64'); ",
            "Remove-Item -LiteralPath $stage,$upload,$source -Force -ErrorAction SilentlyContinue",
        ]),
    ])
}

pub(crate) fn discard_windows_argv(transfer: &TransferId) -> Vec<Arg> {
    powershell(&discard_windows_script(transfer))
}

fn discard_unix_script(transfer: &TransferId) -> Arg {
    Arg::concat(&[
        Arg::joined(&[
            "p=\"$HOME/.cache/domyjob/bin/",
            build_key(),
            "/domyjob.incoming-",
        ]),
        Arg::word(transfer),
        Arg::literal("\"; rm -f \"$p\" \"$p\".*"),
    ])
}

fn install_unix_script(transfer: &TransferId) -> Arg {
    Arg::concat(&[
        Arg::literal("transfer='"),
        Arg::word(transfer),
        Arg::joined(&[
            "'; d=\"$HOME/.cache/domyjob/bin/",
            build_key(),
            "\"; p=\"$d/domyjob.incoming-$transfer\"; mkdir -p \"$d\" && cat > \"$p.$$\" && chmod 755 \"$p.$$\" && mv -f \"$p.$$\" \"$p\"",
        ]),
    ])
}

fn powershell(script: &Arg) -> Vec<Arg> {
    vec![
        Arg::literal("powershell"),
        Arg::literal("-NoProfile"),
        Arg::literal("-NonInteractive"),
        Arg::literal("-InputFormat"),
        Arg::literal("None"),
        Arg::literal("-EncodedCommand"),
        Arg::powershell_encoded(script),
    ]
}

fn cmd(script: Arg) -> Vec<Arg> {
    vec![Arg::literal("cmd"), Arg::literal("/c"), script]
}

impl Remote {
    fn argv(&self) -> Vec<Arg> {
        match self {
            Self::Node(Family::Unix, Placement::Managed) => vec![
                Arg::literal("sh"),
                Arg::literal("-c"),
                Arg::joined(&[
                    "exec \"$HOME/.cache/domyjob/bin/",
                    build_key(),
                    "/domyjob\" node",
                ]),
            ],
            Self::Node(Family::Unix, Placement::Installed) => {
                vec![
                    Arg::literal("sh"),
                    Arg::literal("-c"),
                    Arg::literal(INSTALLED_UNIX),
                ]
            }
            Self::Node(Family::Windows, Placement::Managed) => cmd(Family::Windows.invoke()),
            Self::Node(Family::Windows, Placement::Installed) => {
                cmd(Arg::literal("domyjob node 2>nul"))
            }
            Self::Probe => vec![Arg::literal("uname"), Arg::literal("-sm")],
            Self::WindowsArch => powershell(&Arg::literal(PROBE_WINDOWS)),
            Self::Install(Family::Unix, transfer) => vec![
                Arg::literal("sh"),
                Arg::literal("-c"),
                install_unix_script(transfer),
            ],
            Self::Install(Family::Windows, transfer) => cmd(install_windows_script(transfer)),
            Self::Staged(Family::Unix, transfer) => vec![
                Arg::literal("sh"),
                Arg::literal("-c"),
                Arg::concat(&[
                    Arg::joined(&[
                        "exec \"$HOME/.cache/domyjob/bin/",
                        build_key(),
                        "/domyjob.incoming-",
                    ]),
                    Arg::word(transfer),
                    Arg::literal("\" node"),
                ]),
            ],
            Self::Staged(Family::Windows, transfer) => cmd(staged_windows(transfer)),
            Self::Promote(Family::Unix, transfer) => vec![
                Arg::literal("sh"),
                Arg::literal("-c"),
                Arg::concat(&[
                    Arg::joined(&[
                        "d=\"$HOME/.cache/domyjob/bin/",
                        build_key(),
                        "\"; mv -f \"$d/domyjob.incoming-",
                    ]),
                    Arg::word(transfer),
                    Arg::literal("\" \"$d/domyjob\""),
                ]),
            ],
            Self::Promote(Family::Windows, transfer) => cmd(promote_windows_script(transfer)),
            Self::Discard(Family::Unix, transfer) => vec![
                Arg::literal("sh"),
                Arg::literal("-c"),
                discard_unix_script(transfer),
            ],
            Self::Discard(Family::Windows, transfer) => discard_windows_argv(transfer),
            Self::Uninstall(family, placement) => uninstall_argv(*family, *placement),
            Self::Scrub(family) => scrub_argv(*family),
            Self::Build(Family::Unix, transfer, source) => {
                vec![
                    Arg::literal("sh"),
                    Arg::literal("-c"),
                    build_unix(transfer, source),
                ]
            }
            Self::Build(Family::Windows, transfer, source) => cmd(build_windows(transfer, source)),
        }
    }

    fn text(&self) -> Arg {
        match self {
            Self::Node(Family::Windows, Placement::Managed) => {
                Arg::cmd_wrapped(&Family::Windows.invoke())
            }
            Self::Node(Family::Windows, Placement::Installed) => {
                Arg::cmd_wrapped(&Arg::literal("domyjob node 2>nul"))
            }
            Self::Install(Family::Windows, transfer) => {
                Arg::cmd_wrapped(&install_windows_script(transfer))
            }
            Self::Staged(Family::Windows, transfer) => Arg::cmd_wrapped(&staged_windows(transfer)),
            Self::Promote(Family::Windows, transfer) => {
                Arg::cmd_wrapped(&promote_windows_script(transfer))
            }
            Self::Uninstall(Family::Windows, Placement::Managed) => {
                Arg::cmd_wrapped(&uninstall_windows())
            }
            Self::Uninstall(Family::Windows, Placement::Installed) => {
                Arg::cmd_wrapped(&Arg::literal("domyjob self uninstall --yes"))
            }
            Self::Scrub(Family::Windows) => Arg::cmd_wrapped(&Arg::literal(SCRUB_WINDOWS)),
            Self::Build(Family::Windows, transfer, source) => {
                Arg::cmd_wrapped(&build_windows(transfer, source))
            }
            Self::WindowsArch | Self::Discard(Family::Windows, _) => Arg::spaced(&self.argv()),
            Self::Build(Family::Unix, _, _)
            | Self::Probe
            | Self::Node(Family::Unix, _)
            | Self::Install(Family::Unix, _)
            | Self::Staged(Family::Unix, _)
            | Self::Promote(Family::Unix, _)
            | Self::Discard(Family::Unix, _)
            | Self::Uninstall(Family::Unix, _)
            | Self::Scrub(Family::Unix) => Arg::posix_command(&self.argv()),
        }
    }
}

#[derive(Debug)]
struct Captured {
    status: std::process::ExitStatus,
    out: String,
    errors: String,
}

#[derive(Debug)]
enum Answer {
    Matches(Hello),
    Speaks(crate::protocol::Speaker),
    Silent,
}

#[derive(Debug, Clone)]
pub struct Link<'a> {
    pub machine: Machine,
    config: &'a Config,
    dirs: &'a Dirs,
    family: Family,
    placement: Placement,
}

fn normalize(os: &str, arch: &str) -> (String, String) {
    let os = match os {
        "Linux" => "linux",
        "Darwin" => "macos",
        "FreeBSD" => "freebsd",
        "OpenBSD" => "openbsd",
        "NetBSD" => "netbsd",
        other => other,
    };
    let arch = match arch {
        "arm64" | "Arm64" | "ARM64" => "aarch64",
        "amd64" | "X64" | "AMD64" => "x86_64",
        other => other,
    };
    (os.to_owned(), arch.to_owned())
}

impl<'a> Link<'a> {
    pub fn open(
        config: &'a Config,
        dirs: &'a Dirs,
        machine: &Machine,
    ) -> Result<Self, RemoteError> {
        let transport = config.transport(&machine.transport)?;
        let local = crate::platform::FAMILY;
        let cached = match transport.binary {
            Binary::Itself | Binary::Present => None,
            Binary::Upload => cached_facts(dirs, machine)?,
        };
        let (family, placement) = match &cached {
            Some(facts) => (facts.family(), facts.placement),
            None => (local, Placement::Managed),
        };
        let mut link = Self {
            machine: machine.clone(),
            config,
            dirs,
            family,
            placement,
        };
        match (transport.binary, cached) {
            (Binary::Itself | Binary::Present, _) => {
                let hello = link.hello()?;
                link.remember(hello)?;
            }
            (Binary::Upload, Some(_)) => match link.hello() {
                Ok(hello) if hello.wire.as_raw_str() == wire() => link.remember(hello)?,
                Ok(_)
                | Err(
                    RemoteError::Exited { .. }
                    | RemoteError::Garbled { .. }
                    | RemoteError::Protocol { .. },
                ) => {
                    link.discover()?;
                }
                Err(other) => return Err(other),
            },
            (Binary::Upload, None) => link.discover()?,
        }
        Ok(link)
    }

    fn attempt(&mut self, placement: Placement) -> Result<Answer, RemoteError> {
        self.placement = placement;
        match self.hello() {
            Ok(hello) if hello.wire.as_raw_str() == wire() => Ok(Answer::Matches(hello)),
            Ok(hello) => Ok(Answer::Speaks(crate::protocol::Speaker::of(&hello))),
            Err(RemoteError::Protocol {
                theirs, version, ..
            }) => Ok(Answer::Speaks(crate::protocol::Speaker {
                wire: theirs,
                version,
            })),
            Err(RemoteError::Exited { .. } | RemoteError::Garbled { .. }) => Ok(Answer::Silent),
            Err(other) => Err(other),
        }
    }

    fn discover(&mut self) -> Result<(), RemoteError> {
        let (os, arch) = self.probe()?;
        let mut outdated = None;
        for placement in [Placement::Installed, Placement::Managed] {
            match self.attempt(placement)? {
                Answer::Matches(hello) => {
                    if hello.version.as_raw_str() != VERSION {
                        eprintln!(
                            "domyjob: {} runs domyjob {}, this is {VERSION}; their messages match, and `domyjob self update` there aligns them",
                            self.machine.name, hello.version
                        );
                    }
                    return self.remember(hello);
                }
                Answer::Speaks(speaker) => {
                    if crate::protocol::is_newer(speaker.version.as_raw_str()) {
                        return Err(RemoteError::Newer {
                            machine: self.name(),
                            version: speaker.version,
                        });
                    }
                    outdated = Some(speaker);
                }
                Answer::Silent => {}
            }
        }
        let deliverable = match crate::dist::binary_for(self.config, self.dirs, &os, &arch) {
            Ok(deliverable) => deliverable,
            Err(crate::dist::DistError::NoTrustRoot) if outdated.is_none() => {
                let was = match cached_facts(self.dirs, &self.machine) {
                    Ok(Some(facts)) => format!(" (it last ran {})", facts.hello.version),
                    Ok(None) => String::new(),
                    Err(error) => return Err(error),
                };
                return Err(RemoteError::Unbuilt {
                    machine: self.name(),
                    was,
                });
            }
            Err(cause) => {
                return Err(match outdated {
                    Some(speaker) => RemoteError::Outdated {
                        machine: self.name(),
                        version: speaker.version,
                        cause: Box::new(cause),
                    },
                    None => RemoteError::Dist(cause),
                });
            }
        };
        self.install_for(&deliverable)?;
        match self.attempt(Placement::Managed)? {
            Answer::Matches(hello) => self.remember(hello),
            Answer::Speaks(_) | Answer::Silent => Err(RemoteError::Probe {
                machine: self.name(),
                detail: "the installed copy does not answer".to_owned(),
            }),
        }
    }

    pub fn provision(
        config: &'a Config,
        dirs: &'a Dirs,
        machine: &Machine,
        chosen: Option<Deliverable>,
    ) -> Result<(Self, Hello), RemoteError> {
        let binary = config.transport(&machine.transport)?.binary;
        let family = crate::platform::FAMILY;
        let mut link = Self {
            machine: machine.clone(),
            config,
            dirs,
            family,
            placement: Placement::Managed,
        };
        let hello = match binary {
            Binary::Itself | Binary::Present => link.hello()?,
            Binary::Upload => {
                let (os, arch) = link.probe()?;
                let deliverable = match chosen {
                    Some(deliverable) => deliverable,
                    None => crate::dist::binary_for(config, dirs, &os, &arch)?,
                };
                link.install_for(&deliverable)?
            }
        };
        link.remember(hello.clone())?;
        Ok((link, hello))
    }

    fn witness_path(&self) -> PathBuf {
        witness_path(self.dirs, &self.machine.name)
    }

    fn witness(&self, seen: &crate::audit::Head) -> Result<(), RemoteError> {
        let path = self.witness_path();
        let known: Option<crate::audit::Head> = match crate::state_file::read_json(&path) {
            Ok(known) => known,
            Err(error) => {
                eprintln!(
                    "domyjob: {}: the record of its audit log on this machine is unreadable ({error}); starting a new record from what it reports now",
                    self.name()
                );
                None
            }
        };
        if let Some(known) = known {
            if *seen == known {
                return Ok(());
            }
            if (seen.epoch, seen.seq) < (known.epoch, known.seq) {
                return Err(RemoteError::AuditRolledBack {
                    machine: self.name(),
                    known: known.seq,
                    now: seen.seq,
                });
            }
            let then = if seen.epoch == known.epoch && seen.seq == known.seq {
                Some(seen.hash.clone())
            } else {
                self.call(
                    &Request::AuditAt {
                        epoch: known.epoch,
                        seq: known.seq,
                    },
                    &[],
                )?
                .into_audit_at()
                .map_err(|other| self.unexpected("an audit chain hash", *other))?
            };
            if then.as_ref() != Some(&known.hash) {
                return Err(RemoteError::AuditRewritten {
                    machine: self.name(),
                    seq: known.seq,
                });
            }
        }
        if let Err(error) = save_witness(&path, seen) {
            eprintln!(
                "domyjob: {}: its audit log checks out, but the new position could not be recorded ({error}); the next check starts from the last recorded one",
                self.name()
            );
        }
        Ok(())
    }

    fn remember(&self, hello: Hello) -> Result<(), RemoteError> {
        if hello.wire.as_raw_str() != wire() {
            return Err(RemoteError::Protocol {
                machine: self.machine.name.to_string(),
                theirs: hello.wire,
                version: hello.version,
            });
        }
        let head = self
            .call(&Request::AuditHead, &[])?
            .into_audit_head()
            .map_err(|other| self.unexpected("the audit log's head", *other))?;
        self.witness(&head)?;
        if let Err(error) = save_facts(
            self.dirs,
            &self.machine,
            &Facts {
                hello,
                placement: self.placement,
            },
        ) {
            eprintln!(
                "domyjob: {}: what it reported could not be cached ({error}); it will be asked again next time",
                self.name()
            );
        }
        Ok(())
    }

    fn hello(&self) -> Result<Hello, RemoteError> {
        if let Some(hello) = self.share()? {
            return Ok(hello);
        }
        self.call(&Request::Hello, &[])?
            .into_hello()
            .map_err(|other| self.unexpected("hello", *other))
    }

    fn share(&self) -> Result<Option<Hello>, RemoteError> {
        let transport = self.config.transport(&self.machine.transport)?;
        let Some(template) = transport.sharing() else {
            return Ok(None);
        };
        let Some(_pending) = PendingShare::claim(&self.machine.name)? else {
            return Ok(None);
        };
        let remote = Remote::Node(self.family, self.placement);
        let mut child = self.start(self.command_from(template, &remote)?, Stdio::null())?;
        let activity = crate::liveness::Activity::default();
        let pid = child.id();
        let watchdog =
            crate::liveness::Watchdog::guard(&activity, crate::liveness::LIMITS, move || {
                crate::proc::terminate(pid);
            });
        let held = hold(&mut child, &activity, &self.name());
        let silenced = watchdog.silenced();
        drop(watchdog);
        if silenced {
            match child.wait() {
                Ok(_) | Err(_) => {}
            }
            return Err(self.silent("connecting"));
        }
        let hello = match held {
            Ok(hello) => hello,
            Err(error) => {
                reap_after_failure(&mut child);
                return Err(error);
            }
        };
        let mut shared = match shared_lock(&self.machine.name) {
            Ok(shared) => shared,
            Err(error) => {
                reap_after_failure(&mut child);
                return Err(error);
            }
        };
        shared.insert(self.machine.name.clone(), Some(child));
        drop(shared);
        Ok(Some(hello))
    }

    fn name(&self) -> String {
        self.machine.name.to_string()
    }

    pub fn wipe(&self) -> Result<String, RemoteError> {
        let removed = self.capture(&Remote::Uninstall(self.family, self.placement), &[])?;
        if !removed.status.success() {
            return Err(RemoteError::Refused {
                machine: self.name(),
                refusal: Refusal {
                    code: crate::protocol::RefusalCode::Storage,
                    detail: crate::terminal::RemoteText::new(if removed.errors.is_empty() {
                        removed.out
                    } else {
                        removed.errors
                    }),
                },
            });
        }
        let uploaded = self.config.transport(&self.machine.transport)?.binary == Binary::Upload;
        if uploaded && self.placement == Placement::Managed {
            let scrubbed = self.capture(&Remote::Scrub(self.family), &[])?;
            if !scrubbed.status.success() {
                return Err(RemoteError::Exited {
                    machine: self.name(),
                    doing: "removing its copy of domyjob",
                    status: scrubbed.status,
                    said: said(&scrubbed.errors),
                });
            }
        }
        Ok(removed.out)
    }

    #[must_use]
    pub fn unexpected(&self, expected: &'static str, got: Reply) -> RemoteError {
        match got {
            Reply::Refused(refusal) => RemoteError::Refused {
                machine: self.name(),
                refusal,
            },
            other @ (Reply::Hello(_)
            | Reply::Missing { .. }
            | Reply::Stored { .. }
            | Reply::Job(_)
            | Reply::Jobs { .. }
            | Reply::AuditAt { .. }
            | Reply::AuditHead(_)
            | Reply::Digest(_)
            | Reply::Found(_)
            | Reply::Report(_)
            | Reply::Cleaned(_)
            | Reply::Stream) => RemoteError::Unexpected {
                machine: self.name(),
                expected,
                got: Box::new(other),
            },
        }
    }

    fn command(&self, remote: &Remote) -> Result<Command, RemoteError> {
        let transport = self.config.transport(&self.machine.transport)?;
        self.command_from(transport.command().client(), remote)
    }

    fn command_from(&self, template: &Argv, remote: &Remote) -> Result<Command, RemoteError> {
        let transport = self.config.transport(&self.machine.transport)?;
        let exe = std::env::current_exe().map_err(|source| {
            RemoteError::Io(crate::failure::IoFailure {
                action: "locating",
                path: PathBuf::from("domyjob"),
                source,
            })
        })?;
        let remote_argv = match (transport.binary, remote) {
            (Binary::Itself, Remote::Node(..)) => vec![Arg::path(&exe), Arg::literal("node")],
            (Binary::Itself, Remote::Uninstall(..)) => vec![
                Arg::path(&exe),
                Arg::literal("self"),
                Arg::literal("uninstall"),
                Arg::literal("--yes"),
            ],
            (Binary::Itself | Binary::Upload | Binary::Present, other) => other.argv(),
        };
        let bindings = Bindings::new()
            .with("host", Arg::word(&self.machine.host))
            .with("name", Arg::word(&self.machine.name))
            .with("self", Arg::path(&exe))
            .with("cache", Arg::path(&self.dirs.cache))
            .with("session", Arg::literal(session()))
            .with("home", Arg::path(&self.dirs.home))
            .with("remote", remote.text())
            .with_list("remote_argv", remote_argv);
        let argv = template
            .render(&bindings)
            .map_err(|source| RemoteError::Template {
                transport: self.machine.transport.clone(),
                source,
            })?;
        let invocation =
            crate::spawn::Invocation::from_words(argv).ok_or_else(|| RemoteError::Empty {
                machine: self.name(),
            })?;
        Ok(invocation.command())
    }

    fn spawn(&self, remote: &Remote) -> Result<std::process::Child, RemoteError> {
        self.start(self.command(remote)?, Stdio::piped())
    }

    fn start(
        &self,
        mut command: Command,
        errors: Stdio,
    ) -> Result<std::process::Child, RemoteError> {
        crate::state_file::private_dir(&self.dirs.cache)?;
        let program = command.get_program().to_string_lossy().into_owned();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(errors)
            .spawn()
            .map_err(|source| RemoteError::Start {
                machine: self.name(),
                program,
                source,
            })
    }

    fn capture(&self, remote: &Remote, input: &[u8]) -> Result<Captured, RemoteError> {
        self.capture_telling(remote, input, &|_| {})
    }

    fn capture_telling(
        &self,
        remote: &Remote,
        input: &[u8],
        tell: &(dyn Fn(&str) + Sync),
    ) -> Result<Captured, RemoteError> {
        let mut child = self.spawn(remote)?;
        let pipe = |doing| pipe_error(self.name(), doing);
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(pipe("connecting")(std::io::Error::other("no pipes")));
        };
        let activity = crate::liveness::Activity::default();
        let mut stdin = crate::liveness::Counted::new(stdin, activity.clone());
        let mut stdout = crate::liveness::Counted::new(stdout, activity.clone());
        let mut stderr = BufReader::new(crate::liveness::Counted::new(stderr, activity.clone()));
        let pid = child.id();
        let watchdog =
            crate::liveness::Watchdog::guard(&activity, crate::liveness::LIMITS, move || {
                crate::proc::terminate(pid);
            });
        let (writer, out, errors) = std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                let written = stdin.write_all(input);
                drop(stdin);
                written
            });
            let errors = scope.spawn(move || reported_stderr(&mut stderr, tell));
            let out = crate::bounded::to_end(&mut stdout, crate::bounded::CAPTURE);
            let rest = std::io::copy(&mut stdout, &mut std::io::sink());
            let read = match (out, rest) {
                (Ok(out), Ok(_)) => Ok(out),
                (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            };
            (writer.join(), read, errors.join())
        });
        let status = child.wait().map_err(pipe("reading"))?;
        let silenced = watchdog.silenced();
        drop(watchdog);
        if silenced {
            return Err(self.silent("setting up"));
        }
        let out = out.map_err(pipe("reading"))?;
        let errors = match errors {
            Ok(said) => said.map_err(pipe("reading"))?,
            Err(_panicked) => {
                return Err(pipe("reading")(std::io::Error::other("reader panicked")));
            }
        };
        match writer {
            Ok(Ok(())) => {}
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Ok(Err(e)) => return Err(pipe("sending")(e)),
            Err(_panicked) => {
                return Err(pipe("sending")(std::io::Error::other("writer panicked")));
            }
        }
        Ok(Captured {
            status,
            out: String::from_utf8_lossy(&out).trim().to_owned(),
            errors: errors.trim().to_owned(),
        })
    }

    fn probe(&mut self) -> Result<(String, String), RemoteError> {
        let Captured {
            status,
            out: text,
            errors: said,
        } = self.capture(&Remote::Probe, &[])?;
        let words: Vec<&str> = text.split_whitespace().collect();
        let windows_like = |os: &str| {
            ["MINGW", "MSYS", "CYGWIN", "Windows"]
                .iter()
                .any(|p| os.starts_with(p))
        };
        match words.as_slice() {
            [os, arch] if status.success() && !windows_like(os) => {
                self.family = Family::Unix;
                Ok(normalize(os, arch))
            }
            _ => {
                self.family = Family::Windows;
                let Captured {
                    status: arch_status,
                    out: arch,
                    ..
                } = self.capture(&Remote::WindowsArch, &[])?;
                if !arch_status.success() || arch.is_empty() {
                    return Err(RemoteError::Probe {
                        machine: self.name(),
                        detail: format!(
                            "uname answered {:?} and complained {:?}",
                            crate::terminal::neutralize(&text),
                            crate::terminal::neutralize(&said)
                        ),
                    });
                }
                Ok(normalize("windows", arch.trim()))
            }
        }
    }

    fn install_for(&mut self, deliverable: &Deliverable) -> Result<Hello, RemoteError> {
        let transfer = TransferId::fresh().map_err(RemoteError::TransferId)?;
        let result = self.install_staged(deliverable, &transfer);
        if result.is_err() {
            let _cleanup = self.capture(&Remote::discard(self.family, &transfer), &[]);
        }
        result
    }

    fn install_staged(
        &mut self,
        deliverable: &Deliverable,
        transfer: &TransferId,
    ) -> Result<Hello, RemoteError> {
        let expected = match deliverable.binary() {
            Some(binary) => Some(self.upload(deliverable, binary, transfer)?),
            None => None,
        };
        if let Deliverable::Source { archive, .. } = deliverable {
            self.build(archive, transfer)?;
        }
        let hello = self
            .exchange_with(
                &Remote::staged(self.family, transfer),
                &Request::Hello,
                (&[], &mut std::io::sink()),
            )?
            .into_hello()
            .map_err(|other| self.unexpected("hello", *other))?;
        if let Some(expected) = expected
            && !hello.binary.as_raw_str().eq_ignore_ascii_case(&expected)
        {
            return Err(RemoteError::Tampered {
                machine: self.name(),
                expected,
                reported: hello.binary,
            });
        }
        if matches!(deliverable, Deliverable::Source { .. })
            && hello.build.as_raw_str() != crate::protocol::BUILD_STAMP
        {
            return Err(RemoteError::BuildMismatch {
                machine: self.name(),
                expected: crate::protocol::BUILD_STAMP,
                reported: hello.build,
            });
        }
        let promoted = self.capture(&Remote::promote(self.family, transfer), &[])?;
        if !promoted.status.success() {
            return Err(RemoteError::Exited {
                machine: self.name(),
                doing: "putting the verified binary in place",
                status: promoted.status,
                said: said(&promoted.errors),
            });
        }
        self.placement = Placement::Managed;
        Ok(hello)
    }

    fn upload(
        &self,
        deliverable: &Deliverable,
        binary: &crate::dist::Binary,
        transfer: &TransferId,
    ) -> Result<String, RemoteError> {
        let bytes =
            std::fs::read(binary.path()).map_err(crate::failure::io("reading", binary.path()))?;
        let payload = match self.family {
            Family::Unix => bytes,
            Family::Windows => base64_lines(&bytes).into_bytes(),
        };
        match deliverable {
            Deliverable::Verified(_) => {
                eprintln!(
                    "domyjob: installing domyjob {VERSION} on {}",
                    self.machine.name
                );
            }
            Deliverable::Unsigned { .. } | Deliverable::Source { .. } => {
                eprintln!(
                    "domyjob: WARNING installing an UNSIGNED domyjob on {} because you asked for it; sha256 {}",
                    self.machine.name,
                    binary.sha256()
                );
            }
        }
        self.expect_success(
            &self.capture(&Remote::install(self.family, transfer), &payload)?,
            "staging",
        )?;
        Ok(binary.sha256().to_owned())
    }

    fn build(&self, archive: &[u8], transfer: &TransferId) -> Result<(), RemoteError> {
        let source = BlobId::of(archive);
        eprintln!(
            "domyjob: {}: building domyjob {VERSION} from the sent source",
            self.machine.name
        );
        let payload = match self.family {
            Family::Unix => archive.to_vec(),
            Family::Windows => base64_lines(archive).into_bytes(),
        };
        let name = self.name();
        let tell = move |line: &str| {
            let line = line.trim().trim_start_matches('.').trim_start();
            if line.starts_with("Compiling domyjob") || line.starts_with("Finished") {
                eprintln!("domyjob: {name}: {}", crate::terminal::neutralize(line));
            }
        };
        self.expect_success(
            &self.capture_telling(
                &Remote::build(self.family, transfer, &source),
                &payload,
                &tell,
            )?,
            "building from source",
        )
    }

    fn expect_success(&self, captured: &Captured, doing: &'static str) -> Result<(), RemoteError> {
        if captured.status.success() {
            Ok(())
        } else {
            Err(RemoteError::Exited {
                machine: self.name(),
                doing,
                status: captured.status,
                said: said(&captured.errors),
            })
        }
    }

    pub fn call(
        &self,
        request: &Request,
        blobs: &[(&BlobId, &Origin)],
    ) -> Result<Reply, RemoteError> {
        let mut sink = std::io::sink();
        self.exchange(request, blobs, &mut sink)
    }

    pub fn stream(&self, request: &Request, sink: &mut dyn Write) -> Result<Reply, RemoteError> {
        self.exchange(request, &[], sink)
    }

    fn exchange(
        &self,
        request: &Request,
        blobs: &[(&BlobId, &Origin)],
        sink: &mut dyn Write,
    ) -> Result<Reply, RemoteError> {
        self.exchange_with(
            &Remote::Node(self.family, self.placement),
            request,
            (blobs, sink),
        )
    }

    fn exchange_with(
        &self,
        remote: &Remote,
        request: &Request,
        (blobs, sink): (&[(&BlobId, &Origin)], &mut dyn Write),
    ) -> Result<Reply, RemoteError> {
        let mut child = self.spawn(remote)?;
        let pipe = |doing| pipe_error(self.name(), doing);
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(pipe("connecting")(std::io::Error::other(
                "pipes were not set up",
            )));
        };
        let errors = drain(child.stderr.take());
        let line = serde_json::to_vec(request).map_err(|e| pipe("encoding")(e.into()))?;
        let activity = crate::liveness::Activity::default();
        let stdin = crate::liveness::Counted::new(stdin, activity.clone());
        let stdout = crate::liveness::Counted::new(stdout, activity.clone());
        let pid = child.id();
        let watchdog =
            crate::liveness::Watchdog::guard(&activity, crate::liveness::LIMITS, move || {
                crate::proc::terminate(pid);
            });
        let result = std::thread::scope(|scope| {
            let writer = scope.spawn(move || send_request(stdin, &line, blobs));
            let read = self.read_reply(stdout, sink);
            match writer.join() {
                Ok(Ok(held_open_until_the_reply_was_read)) => {
                    drop(held_open_until_the_reply_was_read);
                    read
                }
                Ok(Err(RemoteError::Pipe { source, .. }))
                    if source.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    read
                }
                Ok(Err(other)) => match read {
                    Ok(_) => Err(other),
                    Err(first) => Err(first),
                },
                Err(_panicked) => match read {
                    Ok(_) => Err(pipe("sending")(std::io::Error::other("writer panicked"))),
                    Err(first) => Err(first),
                },
            }
        });
        let status = child.wait().map_err(pipe("finishing"))?;
        let silenced = watchdog.silenced();
        drop(watchdog);
        if silenced {
            return Err(self.silent("answering"));
        }
        match result {
            Ok(reply) => Ok(reply),
            Err(RemoteError::Garbled { line: garbled, .. })
                if garbled.is_empty() && !status.success() =>
            {
                Err(RemoteError::Exited {
                    machine: self.name(),
                    doing: "answering",
                    status,
                    said: said(&match errors.join() {
                        Ok(text) => text,
                        Err(_panicked) => String::new(),
                    }),
                })
            }
            Err(other) => Err(other),
        }
    }

    fn read_reply(
        &self,
        stdout: impl std::io::Read,
        sink: &mut dyn Write,
    ) -> Result<Reply, RemoteError> {
        receive(&self.name(), &mut BufReader::new(stdout), sink)
    }

    fn silent(&self, doing: &'static str) -> RemoteError {
        RemoteError::Silent {
            machine: self.name(),
            doing,
        }
    }
}

fn hold(
    child: &mut std::process::Child,
    activity: &crate::liveness::Activity,
    machine: &str,
) -> Result<Hello, RemoteError> {
    let (Some(stdin), Some(stdout)) = (child.stdin.as_mut(), child.stdout.take()) else {
        return Err(RemoteError::Pipe {
            machine: machine.to_owned(),
            doing: "opening the shared connection",
            source: std::io::ErrorKind::BrokenPipe.into(),
        });
    };
    let mut line = serde_json::to_vec(&Request::Hold).map_err(|source| RemoteError::Pipe {
        machine: machine.to_owned(),
        doing: "encoding the shared connection request",
        source: std::io::Error::other(source),
    })?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .and_then(|()| stdin.flush())
        .map_err(|source| RemoteError::Pipe {
            machine: machine.to_owned(),
            doing: "requesting the shared connection",
            source,
        })?;
    activity.sent();
    let mut reader = BufReader::new(crate::liveness::Counted::new(stdout, activity.clone()));
    let reply = receive(machine, &mut reader, &mut std::io::sink())?;
    if let Reply::Refused(refusal) = reply {
        return Err(RemoteError::Refused {
            machine: machine.to_owned(),
            refusal,
        });
    }
    reply.into_hello().map_err(|other| RemoteError::Unexpected {
        machine: machine.to_owned(),
        expected: "hello",
        got: other,
    })
}

fn reply_line(reader: &mut dyn std::io::BufRead) -> std::io::Result<Vec<u8>> {
    loop {
        let raw = crate::bounded::line(reader, crate::bounded::REPLY_LINE)?;
        if raw != b"\n" {
            return Ok(raw);
        }
    }
}

pub fn receive(
    machine: &str,
    reader: &mut dyn std::io::BufRead,
    sink: &mut dyn Write,
) -> Result<Reply, RemoteError> {
    let raw = reply_line(reader).map_err(|source| RemoteError::Pipe {
        machine: machine.to_owned(),
        doing: "reading",
        source,
    })?;
    let reply: Reply = match crate::ingress::json(&raw) {
        Ok(reply) => reply,
        Err(error) => {
            return Err(match crate::protocol::Speaker::glimpse(&raw) {
                Some(speaker) if speaker.wire.as_raw_str() != wire() => RemoteError::Protocol {
                    machine: machine.to_owned(),
                    theirs: speaker.wire,
                    version: speaker.version,
                },
                Some(_) | None => RemoteError::Garbled {
                    machine: machine.to_owned(),
                    detail: error.to_string(),
                    line: crate::terminal::neutralize(String::from_utf8_lossy(&raw).trim()),
                },
            });
        }
    };
    if reply != Reply::Stream {
        return Ok(reply);
    }
    match crate::framed::unframe(reader, sink) {
        Ok(_bytes) => Ok(reply),
        Err(crate::framed::Unframed::Failed(refusal)) => Ok(Reply::Refused(refusal)),
        Err(problem) => Err(RemoteError::Stream {
            machine: machine.to_owned(),
            doing: "streaming",
            problem,
        }),
    }
}

fn send_request<W: Write>(
    mut stdin: W,
    line: &[u8],
    blobs: &[(&BlobId, &Origin)],
) -> Result<W, RemoteError> {
    let pipe = |source| RemoteError::Pipe {
        machine: String::new(),
        doing: "sending",
        source,
    };
    stdin.write_all(line).map_err(pipe)?;
    stdin.write_all(b"\n").map_err(pipe)?;
    for (blob, origin) in blobs {
        let bytes = origin.read()?;
        let frame = Frame {
            blob: (*blob).clone(),
            size: crate::domain::len_u64(bytes.len()),
        };
        let mut header = serde_json::to_vec(&frame).map_err(|e| pipe(e.into()))?;
        header.push(b'\n');
        stdin.write_all(&header).map_err(pipe)?;
        stdin.write_all(&bytes).map_err(pipe)?;
    }
    stdin.flush().map_err(pipe)?;
    Ok(stdin)
}

#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    data_encoding::BASE64.encode(bytes)
}

#[must_use]
pub fn base64_lines(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(57) {
        out.push_str(&base64(chunk));
        out.push_str("\r\n");
    }
    out
}

impl crate::ingress::Ingress for Facts {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &std::path::Path) -> Dirs {
        Dirs::isolated_for_test(root)
    }

    #[test]
    fn setup_output_is_reported_line_by_line_and_drained_when_oversized() {
        let said = std::sync::Mutex::new(Vec::new());
        let mut short = std::io::Cursor::new(b"first\nsecond\r\n");
        let reported = reported_stderr(&mut short, &|line| {
            said.lock().unwrap().push(line.to_owned());
        })
        .unwrap();
        assert_eq!(reported, "first\nsecond\n");
        assert_eq!(*said.lock().unwrap(), ["first", "second"]);

        let mut long = std::io::Cursor::new(vec![
            b'x';
            usize::try_from(crate::bounded::CAPTURE).unwrap()
                + 1
        ]);
        let error =
            reported_stderr(&mut long, &|_| panic!("oversized line was reported")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(long.position(), crate::bounded::CAPTURE + 1);
    }

    #[test]
    fn a_crash_while_advancing_a_witness_leaves_an_exact_position() {
        let machine: MachineName = "linux".parse().unwrap();
        let head = |seq: u64, byte: u8| crate::audit::Head {
            epoch: 0,
            seq,
            hash: byte.to_string().repeat(64).parse().unwrap(),
        };
        let old = head(1, 0);
        let new = head(2, 1);
        let steps = {
            let tmp = tempfile::tempdir().unwrap();
            let path = witness_path(&dirs(tmp.path()), &machine);
            save_witness(&path, &old).unwrap();
            let crashing = crate::faults::crash_after(tmp.path(), None);
            save_witness(&path, &new).unwrap();
            crashing.steps()
        };
        for step in 0..steps {
            let tmp = tempfile::tempdir().unwrap();
            let path = witness_path(&dirs(tmp.path()), &machine);
            save_witness(&path, &old).unwrap();
            {
                let _crashing = crate::faults::crash_after(tmp.path(), Some(step));
                match save_witness(&path, &new) {
                    Ok(()) | Err(_) => {}
                }
            }
            let found: crate::audit::Head = crate::state_file::read_json(&path).unwrap().unwrap();
            assert!(found == old || found == new);
        }
    }

    #[test]
    fn a_failed_share_releases_its_claim_without_replacing_an_existing_one() {
        let machine: MachineName = "pending-share-regression".parse().unwrap();
        let pending = PendingShare::claim(&machine).unwrap().unwrap();
        assert!(PendingShare::claim(&machine).unwrap().is_none());
        drop(pending);
        let retried = PendingShare::claim(&machine).unwrap().unwrap();
        assert!(PendingShare::claim(&machine).unwrap().is_none());
        drop(retried);
    }

    #[test]
    fn a_broken_audit_witness_is_not_reported_as_no_witness() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            home: tmp.path().to_path_buf(),
            state: tmp.path().join("state"),
            config: tmp.path().join("config"),
            cache: tmp.path().join("cache"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        };
        let machine: MachineName = "linux".parse().unwrap();
        assert!(witnessed(&dirs, &machine).unwrap().is_none());
        crate::state_file::write_bytes(&witness_path(&dirs, &machine), b"broken").unwrap();
        assert!(matches!(
            witnessed(&dirs, &machine),
            Err(RemoteError::State(_))
        ));
    }

    #[test]
    fn what_a_machine_said_is_its_last_lines_without_ssh_noise_or_escapes() {
        assert_eq!(said(""), "");
        assert_eq!(
            said("Control socket connect(/k/s1-x): Connection refused\n\n"),
            ""
        );
        assert_eq!(
            said("one\ntwo\nControl socket connect(/k): refused\nthree\n\x1b[31mfour\x1b[0m\n"),
            ": two; three; four"
        );
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn probes_normalize_names() {
        assert_eq!(
            normalize("Darwin", "arm64"),
            ("macos".to_owned(), "aarch64".to_owned())
        );
        assert_eq!(
            normalize("windows", "X64"),
            ("windows".to_owned(), "x86_64".to_owned())
        );
        assert_eq!(
            crate::dist::targets("linux", "x86_64")
                .unwrap()
                .first()
                .unwrap()
                .as_str(),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn remote_commands_suit_string_and_argv_transports() {
        let text = |remote: Remote| remote.text().as_arg_str().to_owned();
        assert_eq!(text(Remote::Probe), "uname -sm");
        assert!(
            text(Remote::Node(Family::Unix, Placement::Managed))
                .starts_with("sh -c 'exec \"$HOME/.cache/domyjob/bin/")
        );
        let argv = Remote::Node(Family::Windows, Placement::Managed).argv();
        assert_eq!(argv.first().map(Arg::as_arg_str), Some("cmd"));
        assert!(
            text(Remote::Node(Family::Unix, Placement::Installed)).contains("command -v domyjob")
        );
        assert_eq!(
            text(Remote::Node(Family::Windows, Placement::Installed)),
            "cmd /c \"domyjob node 2>nul\""
        );
        let transfer = TransferId::fresh().unwrap();
        assert!(
            text(Remote::install(Family::Windows, &transfer))
                .starts_with("cmd /c \"mkdir %USERPROFILE%")
        );
        assert!(text(Remote::install(Family::Windows, &transfer)).contains("domyjob.incoming-"));
        assert!(
            text(Remote::promote(Family::Windows, &transfer))
                .contains("move /y domyjob.exe domyjob.old-")
        );
        assert!(text(Remote::staged(Family::Unix, &transfer)).contains("domyjob.incoming"));
        assert!(text(Remote::WindowsArch).contains("-InputFormat None -EncodedCommand"));
        assert_eq!(base64_lines(&[0u8; 60]).lines().count(), 2);
    }

    #[test]
    fn source_build_scripts_require_a_fresh_artifact() {
        let transfer = TransferId::fresh().unwrap();
        let source = BlobId::of(b"source fixture");
        let unix = Remote::build(Family::Unix, &transfer, &source).argv();
        let text = unix.get(2).unwrap().as_arg_str();
        let windows = build_windows_script(&transfer, &source);
        assert!(
            windows_source_build_for_test(std::path::Path::new("build-fixture"), &source)
                .0
                .as_arg_str()
                .contains("findstr . > %TEMP%")
        );
        assert!(text.contains(&format!(
            "DOMYJOB_EXPECTED_BUILD_STAMP={}",
            crate::protocol::BUILD_STAMP
        )));
        assert!(text.contains("--target-dir \"$t\""));
        assert!(text.contains(&format!("build/{}/{source}", build_key())));
        assert!(text.contains("domyjob.incoming-$transfer"));
        assert!(text.contains("MISE_TRUSTED_CONFIG_PATHS=\"$s\""));
        assert!(windows.as_arg_str().contains("$target = Join-Path $c"));
        assert!(
            windows
                .as_arg_str()
                .contains(&format!("build\\{}\\{source}", build_key()))
        );
        assert!(
            windows
                .as_arg_str()
                .contains("MISE_TRUSTED_CONFIG_PATHS = $s")
        );
        assert!(windows.as_arg_str().contains(&format!(
            "DOMYJOB_EXPECTED_BUILD_STAMP = '{}'",
            crate::protocol::BUILD_STAMP
        )));
        assert!(windows.as_arg_str().contains("$job.State -ne 'Completed'"));
        assert!(
            windows
                .as_arg_str()
                .contains("Copy-Item -Force $exe $stage")
        );
    }

    #[test]
    fn separate_transfers_keep_their_own_inputs_and_staged_binaries() {
        let first = TransferId::fresh().unwrap();
        let second = TransferId::fresh().unwrap();
        let source = BlobId::of(b"source fixture");
        let other_source = BlobId::of(b"different source");
        let build = Remote::build(Family::Windows, &first, &source);
        let first_build = build.text();
        assert_eq!(first_build, Arg::cmd_wrapped(build.argv().get(2).unwrap()));
        let second_build = Remote::build(Family::Windows, &second, &source).text();
        assert_ne!(first_build.as_arg_str(), second_build.as_arg_str());
        for family in [Family::Unix, Family::Windows] {
            let same_source_build = Remote::build(family, &first, &source).text();
            let other_build = Remote::build(family, &first, &other_source).text();
            assert_ne!(same_source_build.as_arg_str(), other_build.as_arg_str());
        }
        let install = Remote::install(Family::Windows, &first);
        let first_install = install.text();
        assert_eq!(
            first_install,
            Arg::cmd_wrapped(install.argv().get(2).unwrap())
        );
        let second_install = Remote::install(Family::Windows, &second).text();
        assert_ne!(first_install.as_arg_str(), second_install.as_arg_str());
        for family in [Family::Unix, Family::Windows] {
            let staged = Remote::staged(family, &first).text();
            let promoted = Remote::promote(family, &first).text();
            assert!(staged.as_arg_str().contains(first.as_str()));
            assert!(promoted.as_arg_str().contains(first.as_str()));
            assert!(!staged.as_arg_str().contains(second.as_str()));
        }
    }
}
