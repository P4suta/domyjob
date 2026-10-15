use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

pub(crate) mod native_default;

const EVIDENCE_PARENT: &str = "target/verification-evidence";
const EVIDENCE_ID_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
const HASH_BLOCK_BYTES: usize = 8 * 1024;
#[cfg(unix)]
const EXECUTE_BITS: u32 = 0o111;
const FOREIGN_WRITE_BITS: u32 = 0o022;
const DIAGNOSTIC_BYTES: u64 = 1024 * 1024;
const VERSION_BYTES: u64 = 256;
const LOCAL_LIMIT_MILLIS: u64 = 10_000;
const POLL_MILLIS: u64 = 10;

#[derive(Clone, Copy)]
pub(crate) enum EvidenceKind {
    ProofAttempt,
    SolverObservation,
}

impl EvidenceKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::ProofAttempt => "proof-attempt",
            Self::SolverObservation => "solver-observation",
        }
    }
}

fn new_evidence(parent: &Path, kind: EvidenceKind) -> Result<PathBuf, String> {
    let mut identity = [0; EVIDENCE_ID_BYTES];
    getrandom::fill(&mut identity).map_err(|error| error.to_string())?;
    claimed_evidence(parent, kind, &identity)
}

fn claimed_evidence(
    parent: &Path,
    kind: EvidenceKind,
    identity: &[u8; EVIDENCE_ID_BYTES],
) -> Result<PathBuf, String> {
    let metadata = std::fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || parent.canonicalize().map_err(|error| error.to_string())? != parent
    {
        return Err("evidence parent is not an owned physical directory".to_owned());
    }
    let name = format!(
        "{}-{}",
        kind.prefix(),
        data_encoding::HEXLOWER.encode(identity)
    );
    let _claim = crate::raw::create_new_file(&parent.join(format!("{name}.claim")))
        .map_err(|error| format!("evidence identity is already claimed: {error}"))?;
    let destination = parent.join(&name);
    match std::fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) | Err(_) => return Err("claimed evidence destination already exists".to_owned()),
    }
    directory(parent, &name, true)
}

pub(crate) fn evidence(root: &Path, kind: EvidenceKind) -> Result<PathBuf, String> {
    new_evidence(&directory(root, EVIDENCE_PARENT, true)?, kind)
}

fn directory(root: &Path, relative: &str, create: bool) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let mut current = root.clone();
    for component in Path::new(relative).components() {
        let Component::Normal(component) = component else {
            return Err("invalid fixed proof tool path component".to_owned());
        };
        current.push(component);
        if create && !current.try_exists().map_err(|error| error.to_string())? {
            crate::raw::create_dir_all(&current).map_err(|error| error.to_string())?;
        }
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| error.to_string())?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || current.canonicalize().map_err(|error| error.to_string())? != current
        {
            return Err("proof tool parent changed physical identity".to_owned());
        }
    }
    if current.starts_with(root) {
        Ok(current)
    } else {
        Err("proof tool parent escaped the owned source".to_owned())
    }
}

fn regular(path: &Path) -> Result<std::fs::Metadata, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.is_file()
        && !metadata.file_type().is_symlink()
        && path.canonicalize().map_err(|error| error.to_string())? == path
    {
        Ok(metadata)
    } else {
        Err("proof tool input is not a physical regular file".to_owned())
    }
}

fn bounded_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let before = regular(path)?;
    if before.len() > maximum {
        return Err("proof tool input exceeds its fixed byte bound".to_owned());
    }
    let bound = maximum
        .checked_add(1)
        .ok_or_else(|| "proof tool read bound overflow".to_owned())?;
    let bound = usize::try_from(bound).map_err(|error| error.to_string())?;
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let bytes = Read::bytes(std::io::BufReader::new(file))
        .take(bound)
        .collect::<std::io::Result<Vec<u8>>>()
        .map_err(|error| error.to_string())?;
    let after = regular(path)?;
    let length = u64::try_from(bytes.len()).map_err(|error| error.to_string())?;
    if length <= maximum && before.len() == after.len() && after.len() == length {
        Ok(bytes)
    } else {
        Err("proof tool input changed during bounded reading".to_owned())
    }
}

#[derive(Clone, Copy)]
struct CapturePlan<'a> {
    evidence: &'a Path,
    name: &'a str,
    limit_millis: u64,
    stdout_bytes: u64,
}

impl CapturePlan<'_> {
    fn paths(self) -> (PathBuf, PathBuf) {
        (
            self.evidence.join(format!("{}.stdout", self.name)),
            self.evidence.join(format!("{}.stderr", self.name)),
        )
    }
}

const fn output_bounds(stdout: u64, stderr: u64, limit: u64) -> bool {
    stdout <= limit && stderr <= DIAGNOSTIC_BYTES
}

fn deadline(elapsed_millis: u128, limit_millis: u64) -> bool {
    elapsed_millis < u128::from(limit_millis)
}

#[derive(Clone, Copy, Debug)]
struct OutputObservation {
    stdout_bytes: u64,
    stderr_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct CaptureTime {
    elapsed_millis: u128,
    limit_millis: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureReason {
    Deadline,
    OutputBound,
    AbnormalExit,
    ProcessObservation,
    OutputObservation,
}

#[derive(Debug)]
struct CaptureObservation {
    reasons: Vec<CaptureReason>,
    identity: u32,
    time: CaptureTime,
    state: Result<Option<ExitStatus>, String>,
    output: Result<OutputObservation, String>,
    stdout_limit: u64,
    stderr_limit: u64,
    kill: Option<Result<(), String>>,
    wait: Result<ExitStatus, String>,
    stdout: PathBuf,
    stderr: PathBuf,
}

#[derive(Debug)]
enum CaptureError {
    BeforeSpawn(String),
    Child(Box<CaptureObservation>),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeSpawn(error) => formatter.write_str(error),
            Self::Child(failure) => write!(
                formatter,
                "child {}: reasons {:?}; elapsed {} ms, limit {} ms; state {:?}; output {:?}, stdout cap {}, stderr cap {}; kill {:?}; settle {:?}; raw stdout {}; stderr {}",
                failure.identity,
                failure.reasons,
                failure.time.elapsed_millis,
                failure.time.limit_millis,
                failure.state,
                failure.output,
                failure.stdout_limit,
                failure.stderr_limit,
                failure.kill,
                failure.wait,
                failure.stdout.display(),
                failure.stderr.display()
            ),
        }
    }
}

fn observed_output(stdout: &Path, stderr: &Path) -> Result<OutputObservation, String> {
    Ok(OutputObservation {
        stdout_bytes: regular(stdout)?.len(),
        stderr_bytes: regular(stderr)?.len(),
    })
}

fn capture_reasons(
    state: &Result<Option<ExitStatus>, String>,
    output: &Result<OutputObservation, String>,
    timing: (CaptureTime, u64),
) -> Vec<CaptureReason> {
    let (time, stdout_limit) = timing;
    let mut reasons = Vec::new();
    if !deadline(time.elapsed_millis, time.limit_millis) {
        reasons.push(CaptureReason::Deadline);
    }
    match state {
        Ok(Some(status)) if !status.success() => reasons.push(CaptureReason::AbnormalExit),
        Err(_) => reasons.push(CaptureReason::ProcessObservation),
        Ok(Some(_) | None) => {}
    }
    match output {
        Ok(observed)
            if !output_bounds(observed.stdout_bytes, observed.stderr_bytes, stdout_limit) =>
        {
            reasons.push(CaptureReason::OutputBound);
        }
        Err(_) => reasons.push(CaptureReason::OutputObservation),
        Ok(_) => {}
    }
    reasons
}

fn capture_observed(
    command: &mut Command,
    plan: CapturePlan<'_>,
) -> Result<CaptureObservation, CaptureError> {
    let (stdout, stderr) = plan.paths();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(
            crate::raw::create_new_file(&stdout)
                .map_err(|error| CaptureError::BeforeSpawn(error.to_string()))?,
        )
        .stderr(
            crate::raw::create_new_file(&stderr)
                .map_err(|error| CaptureError::BeforeSpawn(error.to_string()))?,
        )
        .spawn()
        .map_err(|error| {
            CaptureError::BeforeSpawn(format!("tool {} did not start: {error}", plan.name))
        })?;
    let identity = child.id();
    let elapsed = raw::clock();
    loop {
        let state = child.try_wait().map_err(|error| error.to_string());
        let output = observed_output(&stdout, &stderr);
        let time = CaptureTime {
            elapsed_millis: elapsed(),
            limit_millis: plan.limit_millis,
        };
        let reasons = capture_reasons(&state, &output, (time, plan.stdout_bytes));
        if !reasons.is_empty() {
            let kill = match &state {
                Ok(Some(_)) => None,
                Ok(None) | Err(_) => Some(child.kill().map_err(|error| error.to_string())),
            };
            let wait = child.wait().map_err(|error| error.to_string());
            return Err(CaptureError::Child(Box::new(CaptureObservation {
                reasons,
                identity,
                time,
                state,
                output,
                stdout_limit: plan.stdout_bytes,
                stderr_limit: DIAGNOSTIC_BYTES,
                kill,
                wait,
                stdout,
                stderr,
            })));
        }
        if let Ok(Some(status)) = &state {
            let wait = child.wait().map_err(|error| error.to_string());
            let settled = matches!(&wait, Ok(reaped) if reaped == status);
            let observation = CaptureObservation {
                reasons: if settled {
                    Vec::new()
                } else {
                    vec![CaptureReason::ProcessObservation]
                },
                identity,
                time,
                state,
                output,
                stdout_limit: plan.stdout_bytes,
                stderr_limit: DIAGNOSTIC_BYTES,
                kill: None,
                wait,
                stdout,
                stderr,
            };
            return if settled {
                Ok(observation)
            } else {
                Err(CaptureError::Child(Box::new(observation)))
            };
        }
        raw::sleep_millis(POLL_MILLIS);
    }
}

fn capture(command: &mut Command, plan: CapturePlan<'_>) -> Result<PathBuf, String> {
    capture_observed(command, plan)
        .map(|observed| observed.stdout)
        .map_err(|error| error.to_string())
}

#[derive(PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultSelection {
    target: String,
    mise: SelectedProgram,
    cargo_proxy: SelectedProgram,
    standalone_proxy: SelectedProgram,
    outer_cargo: SelectedProgram,
    observer: PathBuf,
    bundle: PathBuf,
    toolchain_link: SelectionLink,
    toolchain: PathBuf,
    environment: Vec<(String, Option<[u8; HASH_BYTES]>)>,
}

impl std::fmt::Debug for DefaultSelection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DefaultSelection")
            .field("target", &self.target)
            .field("bundle", &self.bundle)
            .field("toolchain", &self.toolchain)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedProgram {
    selected: PathBuf,
    canonical: PathBuf,
    links: Vec<SelectionLink>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionLink {
    path: PathBuf,
    target: PathBuf,
    device: u64,
    inode: u64,
    bytes: u64,
    mode: u32,
    owner: u32,
    group: u32,
}

#[cfg(unix)]
fn observed_link(path: &Path) -> Result<SelectionLink, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_symlink() {
        return Err("missing native selection link".to_owned());
    }
    let target = std::fs::read_link(path).map_err(|error| error.to_string())?;
    let after = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.dev() != after.dev()
        || metadata.ino() != after.ino()
        || metadata.uid() != after.uid()
        || metadata.gid() != after.gid()
        || metadata.mode() != after.mode()
        || metadata.len() != after.len()
    {
        return Err("native selection link changed while observed".to_owned());
    }
    Ok(SelectionLink {
        path: path.to_path_buf(),
        target,
        device: after.dev(),
        inode: after.ino(),
        bytes: after.len(),
        mode: after.mode(),
        owner: after.uid(),
        group: after.gid(),
    })
}

#[cfg(not(unix))]
fn observed_link(_path: &Path) -> Result<SelectionLink, String> {
    Err("unsupported native selection link semantics".to_owned())
}

fn selection_link(path: &Path) -> Result<SelectedProgram, String> {
    let mut remaining = path.to_path_buf();
    let mut links = Vec::new();
    loop {
        let mut current = PathBuf::new();
        let mut redirected = None;
        let mut components = remaining.components();
        while let Some(component) = components.next() {
            current.push(component.as_os_str());
            let metadata =
                std::fs::symlink_metadata(&current).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                let link = observed_link(&current)?;
                if links.len()
                    >= usize::try_from(native_default::MAX_MEMBERS)
                        .map_err(|error| error.to_string())?
                    || links
                        .iter()
                        .any(|previous: &SelectionLink| previous.path == link.path)
                {
                    return Err(
                        "native selection link chain is cyclic or exceeds its bound".to_owned()
                    );
                }
                let mut target = if link.target.is_absolute() {
                    link.target.clone()
                } else {
                    current
                        .parent()
                        .ok_or_else(|| "selection link lacks a parent".to_owned())?
                        .join(&link.target)
                };
                target.extend(components);
                links.push(link);
                redirected = Some(target);
                break;
            }
        }
        match redirected {
            Some(target) => remaining = target,
            None => break,
        }
    }
    Ok(SelectedProgram {
        selected: path.to_path_buf(),
        canonical: path.canonicalize().map_err(|error| error.to_string())?,
        links,
    })
}

impl DefaultSelection {
    fn target(&self) -> Result<native_default::Target, String> {
        match self.target.as_str() {
            "x86_64-unknown-linux-gnu" => Ok(native_default::Target::LinuxAmd64),
            "aarch64-apple-darwin" => Ok(native_default::Target::MacArm64),
            _ => Err("unsupported native-default target".to_owned()),
        }
    }
}

fn default_target() -> Result<native_default::Target, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok(native_default::Target::LinuxAmd64),
        ("macos", "aarch64") => Ok(native_default::Target::MacArm64),
        _ => Err("native-default observations require Linux amd64 or Mac ARM64".to_owned()),
    }
}

fn physical_directory(path: &Path) -> Result<PathBuf, String> {
    let canonical = path.canonicalize().map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| error.to_string())?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(canonical)
    } else {
        Err("native-default parent is not a physical directory".to_owned())
    }
}

fn selected_program(name: &str) -> Result<SelectedProgram, String> {
    let path = std::env::var_os("PATH").ok_or_else(|| "missing tool-selection PATH".to_owned())?;
    for parent in std::env::split_paths(&path) {
        let candidate = parent.join(name);
        if let Ok(metadata) = candidate.metadata()
            && metadata.is_file()
            && executable_metadata(&metadata)
        {
            return selection_link(&candidate);
        }
    }
    Err(format!("missing selected native-default program: {name}"))
}

#[cfg(unix)]
fn executable_metadata(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & EXECUTE_BITS != 0
}

#[cfg(not(unix))]
const fn executable_metadata(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn default_selection() -> Result<DefaultSelection, String> {
    let target = default_target()?;
    for name in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            return Err(format!(
                "unsupported nonempty native-default execution control: {name}"
            ));
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "missing native tool home".to_owned())?;
    let parent = match std::env::var("KANI_HOME") {
        Ok(parent) => PathBuf::from(parent),
        Err(std::env::VarError::NotPresent) => PathBuf::from(home).join(".kani"),
        Err(error @ std::env::VarError::NotUnicode(_)) => return Err(error.to_string()),
    };
    let parent = physical_directory(&parent)?;
    for entry in std::fs::read_dir(&parent).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".tar.gz"))
        {
            return Err("native-default bundle retains an incomplete setup archive".to_owned());
        }
    }
    let bundle = physical_directory(&parent.join("kani-0.68.0"))?;
    let link = bundle.join("toolchain");
    let toolchain_link = observed_link(&link)?;
    let toolchain = physical_directory(&link)?;
    let environment = [
        "PATH",
        "HOME",
        "KANI_HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "PYTHONPATH",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_owned(),
            std::env::var_os(name).map(|value| Sha256::digest(value.as_encoded_bytes()).into()),
        )
    })
    .collect();
    Ok(DefaultSelection {
        target: target.triple().to_owned(),
        mise: selected_program("mise")?,
        cargo_proxy: selected_program("cargo-kani")?,
        standalone_proxy: selected_program("kani")?,
        outer_cargo: selected_program("cargo")?,
        observer: std::env::current_exe().map_err(|error| error.to_string())?,
        bundle,
        toolchain_link,
        toolchain,
        environment,
    })
}

pub(crate) fn emit_default_selection() -> Result<(), String> {
    let context = default_selection()?;
    let bytes = serde_json::to_vec(&context).map_err(|error| error.to_string())?;
    emit_default_bytes(&bytes)
}

fn emit_default_bytes(bytes: &[u8]) -> Result<(), String> {
    if u64::try_from(bytes.len()).map_err(|error| error.to_string())? > DIAGNOSTIC_BYTES {
        return Err("native-default selection exceeds its observation bound".to_owned());
    }
    let mut output = raw::stdout().lock();
    output.write_all(bytes).map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

pub(crate) fn isolate_native_default(command: &mut Command) {
    command
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH");
}

#[derive(Clone, Copy)]
enum DefaultObserver {
    Context,
    Versions,
}

fn default_observer_command(root: &Path, observer: DefaultObserver) -> Result<Command, String> {
    let tag = match observer {
        DefaultObserver::Context => "__native-default-context",
        DefaultObserver::Versions => "__native-default-versions",
    };
    let mut command = crate::raw::command("mise");
    command
        .current_dir(root)
        .args(["x", "cargo:kani-verifier@0.68.0", "--"])
        .arg(std::env::current_exe().map_err(|error| error.to_string())?)
        .args(["verify", tag])
        .env("CARGO_BUILD_JOBS", "2")
        .env("MISE_INSTALL_MISSING_TOOLS", "false");
    isolate_native_default(&mut command);
    Ok(command)
}

fn default_context(root: &Path, evidence: &Path) -> Result<DefaultSelection, String> {
    let observation = new_evidence(evidence, EvidenceKind::SolverObservation)?;
    let provider = default_lookup(
        root,
        &observation,
        "provider",
        &["where", "cargo:kani-verifier@0.68.0"],
    )?;
    let proxy = default_lookup(
        root,
        &observation,
        "proxy",
        &[
            "which",
            "cargo-kani",
            "--tool",
            "cargo:kani-verifier@0.68.0",
        ],
    )?;
    let mut command = default_observer_command(root, DefaultObserver::Context)?;
    let output = capture(
        &mut command,
        CapturePlan {
            evidence: &observation,
            name: "default-context",
            limit_millis: LOCAL_LIMIT_MILLIS,
            stdout_bytes: DIAGNOSTIC_BYTES,
        },
    )?;
    let context: DefaultSelection = domyjob_core::ingress::json(
        &bounded_bytes(&output, DIAGNOSTIC_BYTES)?,
        usize::try_from(DIAGNOSTIC_BYTES).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let provider = physical_directory(&provider)?;
    let expected = provider
        .join("bin/cargo-kani")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if context.cargo_proxy.canonical == expected
        && proxy.canonicalize().map_err(|error| error.to_string())? == expected
        && context.standalone_proxy.canonical
            == provider
                .join("bin/kani")
                .canonicalize()
                .map_err(|error| error.to_string())?
    {
        Ok(context)
    } else {
        Err("native-default child PATH disagrees with its exact mise provider".to_owned())
    }
}

fn default_lookup(
    root: &Path,
    evidence: &Path,
    name: &str,
    arguments: &[&str],
) -> Result<PathBuf, String> {
    let mut command = crate::raw::command("mise");
    command
        .current_dir(root)
        .args(arguments)
        .env("MISE_INSTALL_MISSING_TOOLS", "false");
    let output = capture(
        &mut command,
        CapturePlan {
            evidence,
            name,
            limit_millis: LOCAL_LIMIT_MILLIS,
            stdout_bytes: VERSION_BYTES,
        },
    )?;
    let bytes = bounded_bytes(&output, VERSION_BYTES)?;
    let path = std::str::from_utf8(&bytes)
        .map_err(|error| error.to_string())?
        .trim();
    if path.is_empty() || path.contains(['\n', '\r', '\0']) {
        Err("invalid exact native-default tool path".to_owned())
    } else {
        Ok(PathBuf::from(path))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DefaultFile {
    path: PathBuf,
    identity: native_default::Identity,
}

#[cfg(unix)]
fn default_directory(path: &Path) -> Result<DefaultFile, String> {
    use std::os::unix::fs::MetadataExt;
    let canonical = physical_directory(path)?;
    if canonical != path {
        return Err("native-default directory changed canonical identity".to_owned());
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.mode() & FOREIGN_WRITE_BITS != 0 {
        return Err("native-default directory is writable by another user".to_owned());
    }
    Ok(DefaultFile {
        path: path.to_path_buf(),
        identity: native_default::Identity {
            path: Sha256::digest(path.as_os_str().as_encoded_bytes()).into(),
            content: [0; HASH_BYTES],
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            mode: metadata.mode(),
            owner: metadata.uid(),
            group: metadata.gid(),
        },
    })
}

#[cfg(not(unix))]
fn default_directory(_path: &Path) -> Result<DefaultFile, String> {
    Err("unsupported native-default directory identity semantics".to_owned())
}

#[cfg(unix)]
fn default_metadata_identity(metadata: &std::fs::Metadata) -> native_default::Identity {
    use std::os::unix::fs::MetadataExt;
    native_default::Identity {
        path: [0; HASH_BYTES],
        content: [0; HASH_BYTES],
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.len(),
        mode: metadata.mode(),
        owner: metadata.uid(),
        group: metadata.gid(),
    }
}

struct DefaultObjectMetadata {
    metadata: std::fs::Metadata,
    identity: native_default::Identity,
}

#[cfg(unix)]
fn default_object_metadata(
    path: &Path,
    file: Option<&std::fs::File>,
) -> Result<DefaultObjectMetadata, String> {
    let metadata = match file {
        Some(file) => file.metadata().map_err(|error| error.to_string())?,
        None => regular(path)?,
    };
    Ok(DefaultObjectMetadata {
        identity: default_metadata_identity(&metadata),
        metadata,
    })
}

#[cfg(not(unix))]
fn default_object_metadata(
    _path: &Path,
    _file: Option<&std::fs::File>,
) -> Result<DefaultObjectMetadata, String> {
    Err("unsupported native-default physical object semantics".to_owned())
}

fn default_file(
    path: &Path,
    target: native_default::Target,
    kind: native_default::Object,
) -> Result<DefaultFile, String> {
    let before = default_object_metadata(path, None)?;
    if before.metadata.len() > native_default::MAX_OBJECT_BYTES
        || before.identity.mode & FOREIGN_WRITE_BITS != 0
    {
        return Err(
            "native-default object exceeds its bound or is writable by another user".to_owned(),
        );
    }
    if kind == native_default::Object::Executable && !executable_metadata(&before.metadata) {
        return Err("native-default tool is not executable".to_owned());
    }
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let opened = default_object_metadata(path, Some(&file))?;
    if !native_default::unchanged(&before.identity, &opened.identity) {
        return Err("native-default open changed object identity".to_owned());
    }
    let mut input = file.take(
        native_default::MAX_OBJECT_BYTES
            .checked_add(1)
            .ok_or_else(|| "tool read bound overflow".to_owned())?,
    );
    let mut block = [0; HASH_BLOCK_BYTES];
    let mut prefix = Vec::new();
    let mut length = 0_u64;
    let mut hash = Sha256::new();
    loop {
        let count = input.read(&mut block).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        let bytes = block
            .get(..count)
            .ok_or_else(|| "invalid native tool read length".to_owned())?;
        length = length
            .checked_add(u64::try_from(count).map_err(|error| error.to_string())?)
            .ok_or_else(|| "native tool length overflow".to_owned())?;
        if length > native_default::MAX_OBJECT_BYTES {
            return Err("native tool grew beyond its object cap".to_owned());
        }
        let remaining = native_default::HEADER_PREFIX_BYTES
            .checked_sub(prefix.len())
            .ok_or_else(|| "invalid native header count".to_owned())?;
        prefix.extend(bytes.iter().take(remaining));
        hash.update(bytes);
    }
    let after = default_object_metadata(path, None)?;
    let descriptor = default_object_metadata(path, Some(input.get_ref()))?;
    if after.metadata.len() != length
        || !native_default::unchanged(&before.identity, &after.identity)
        || !native_default::unchanged(&descriptor.identity, &after.identity)
    {
        return Err("native-default object changed during its bounded read".to_owned());
    }
    native_default::object(target, kind, &prefix, length)
        .map_err(|error| format!("native object refusal: {error:?}"))?;
    Ok(DefaultFile {
        path: path.to_path_buf(),
        identity: native_default::Identity {
            path: Sha256::digest(path.as_os_str().as_encoded_bytes()).into(),
            content: hash.finalize().into(),
            bytes: length,
            ..after.identity
        },
    })
}

#[derive(Eq)]
struct DefaultFiles {
    files: Vec<DefaultFile>,
    directories: Vec<DefaultFile>,
    absent_directories: Vec<PathBuf>,
    roles: native_default::Roles,
    members: u32,
    bytes: u64,
}

impl std::fmt::Debug for DefaultFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DefaultFiles")
            .field("members", &self.members)
            .field("bytes", &self.bytes)
            .field("roles", &self.roles)
            .field("files", &self.files.len())
            .field("directories", &self.directories.len())
            .field("absent_directories", &self.absent_directories.len())
            .finish()
    }
}

fn same_default_file(left: &DefaultFile, right: &DefaultFile) -> bool {
    left.path == right.path && native_default::unchanged(&left.identity, &right.identity)
}

impl PartialEq for DefaultFiles {
    fn eq(&self, other: &Self) -> bool {
        self.roles.mask() == other.roles.mask()
            && self.absent_directories == other.absent_directories
            && self.members == other.members
            && self.bytes == other.bytes
            && self.files.len() == other.files.len()
            && self.directories.len() == other.directories.len()
            && self
                .files
                .iter()
                .zip(&other.files)
                .all(|(left, right)| same_default_file(left, right))
            && self
                .directories
                .iter()
                .zip(&other.directories)
                .all(|(left, right)| same_default_file(left, right))
    }
}

fn default_optional_directory(snapshot: &mut DefaultFiles, path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => snapshot.directories.push(default_directory(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            snapshot.absent_directories.push(path.to_path_buf());
        }
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

fn include_default_file(
    snapshot: &mut DefaultFiles,
    budget: &mut native_default::Budget,
    context: &DefaultSelection,
    candidate: (&Path, native_default::Object, native_default::Role),
) -> Result<(), String> {
    let (path, kind, role) = candidate;
    let roles = snapshot
        .roles
        .include(role)
        .map_err(|error| format!("native-default role refusal: {error:?}"))?;
    let length = regular(path)?.len();
    let admitted = native_default::Budget::from_observation(budget.members(), budget.bytes())
        .ok_or_else(|| "invalid native-default budget checkpoint".to_owned())?;
    let next = admitted
        .include(length)
        .map_err(|error| format!("native-default tool budget: {error:?}"))?;
    let observed = default_file(path, context.target()?, kind)?;
    if observed.identity.bytes != length {
        return Err("native-default object changed between reservation and capture".to_owned());
    }
    snapshot.files.push(observed);
    snapshot.roles = roles;
    *budget = next;
    Ok(())
}

fn default_support_directory(
    snapshot: &mut DefaultFiles,
    budget: &mut native_default::Budget,
    context: &DefaultSelection,
    path: &Path,
) -> Result<(), String> {
    let canonical = physical_directory(path)?;
    if canonical != path {
        return Err("native-default sysroot directory changed physical identity".to_owned());
    }
    snapshot.directories.push(default_directory(&canonical)?);
    for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let member = entry.path();
        if member.file_name().is_some_and(|name| name == "rustlib") {
            continue;
        }
        let kind = match member.extension().and_then(|extension| extension.to_str()) {
            Some("rlib") => native_default::Object::Support,
            Some("so" | "dylib") => native_default::Object::Library,
            _ => {
                return Err(
                    "unregistered member in native-default current-target sysroot".to_owned(),
                );
            }
        };
        include_default_file(
            snapshot,
            budget,
            context,
            (&member, kind, native_default::Role::Support),
        )?;
    }
    Ok(())
}

fn default_required_files(
    context: &DefaultSelection,
) -> Result<Vec<(PathBuf, native_default::Object, native_default::Role)>, String> {
    use native_default::{Object, Role};
    let target = context.target()?;
    let mut required = Vec::new();
    for (path, role) in [
        (&context.mise.canonical, Role::Mise),
        (&context.observer, Role::Observer),
        (&context.outer_cargo.canonical, Role::OuterCargo),
        (&context.cargo_proxy.canonical, Role::CargoProxy),
        (&context.standalone_proxy.canonical, Role::StandaloneProxy),
    ] {
        required.push((path.clone(), Object::Executable, role));
    }
    for (name, role) in [
        ("kani-driver", Role::Driver),
        ("kani-compiler", Role::Compiler),
        ("cbmc", Role::Cbmc),
        ("goto-cc", Role::GotoCc),
        ("goto-instrument", Role::GotoInstrument),
    ] {
        required.push((
            context.bundle.join("bin").join(name),
            Object::Executable,
            role,
        ));
    }
    for (name, role) in [("cargo", Role::LinkedCargo), ("rustc", Role::LinkedRustc)] {
        required.push((
            context.toolchain.join("bin").join(name),
            Object::Executable,
            role,
        ));
    }
    let runtime = match target {
        native_default::Target::LinuxAmd64 => "librustc_driver-59857489213b2a36.so",
        native_default::Target::MacArm64 => "librustc_driver-83914c2c3aa68f2a.dylib",
    };
    required.push((
        context.toolchain.join("lib").join(runtime),
        Object::Library,
        Role::RustcRuntime,
    ));
    let llvm = match target {
        native_default::Target::LinuxAmd64 => "libLLVM.so.23.1-rust-1.100.0-nightly",
        native_default::Target::MacArm64 => "libLLVM.dylib",
    };
    required.push((
        context.toolchain.join("lib").join(llvm),
        Object::Library,
        Role::LlvmRuntime,
    ));
    for (name, role) in [
        ("rustc-version", Role::CompilerMarker),
        ("rust-toolchain-version", Role::ToolchainMarker),
        ("library/kani/kani_lib.c", Role::KaniLibC),
    ] {
        required.push((context.bundle.join(name), Object::Support, role));
    }
    Ok(required)
}

fn default_files(context: &DefaultSelection) -> Result<DefaultFiles, String> {
    let target = context.target()?;
    let mut snapshot = DefaultFiles {
        files: Vec::new(),
        directories: Vec::new(),
        absent_directories: Vec::new(),
        roles: native_default::Roles::from_observation(0),
        members: 0,
        bytes: 0,
    };
    let mut budget = native_default::Budget::new();
    for (path, kind, role) in default_required_files(context)? {
        include_default_file(&mut snapshot, &mut budget, context, (&path, kind, role))?;
    }
    let library = context.bundle.join("lib");
    default_support_directory(&mut snapshot, &mut budget, context, &library)?;
    default_support_directory(
        &mut snapshot,
        &mut budget,
        context,
        &library.join("rustlib").join(target.triple()).join("lib"),
    )?;
    snapshot
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    snapshot.members = budget.members();
    snapshot.bytes = budget.bytes();
    for path in [
        &context.bundle.join("pyroot"),
        &context.bundle.join("pyroot/bin"),
    ] {
        default_optional_directory(&mut snapshot, path)?;
    }
    for path in [
        &context.bundle,
        &context.toolchain,
        &context.bundle.join("bin"),
        &context.toolchain.join("bin"),
        &context.toolchain.join("lib"),
        &library.join("rustlib"),
        &library.join("rustlib").join(target.triple()),
    ] {
        snapshot.directories.push(default_directory(path)?);
    }
    snapshot
        .directories
        .sort_by(|left, right| left.path.cmp(&right.path));
    if !snapshot.roles.admits() {
        return Err("native-default snapshot lacks an actual required role".to_owned());
    }
    Ok(snapshot)
}

pub(crate) const DEFAULT_COMPILER_VERSION: &str = "rustc 1.100.0-nightly (8925ea358 2026-08-20)";
const DEFAULT_CARGO_VERSION: &str = "Kani Rust Verifier 0.68.0 (cargo plugin)";
const DEFAULT_STANDALONE_VERSION: &str = "Kani Rust Verifier 0.68.0 (standalone)";
const DEFAULT_CBMC_VERSION: &str = "6.11.0 (cbmc-6.11.0)";

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefaultVersions {
    pub(crate) kani: String,
    pub(crate) compiler: String,
    pub(crate) cbmc: String,
    pub(crate) goto_cc: String,
    pub(crate) goto_instrument: String,
    mise: String,
    outer_cargo: String,
    linked_cargo: String,
    linked_rustc: String,
    standalone: String,
    observations: Vec<DefaultVersionRecord>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultVersionRecord {
    role: String,
    raw: String,
    normal_code: i32,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultVersionObservation {
    before: DefaultSelection,
    after: DefaultSelection,
    versions: DefaultVersions,
}

impl DefaultVersionObservation {
    fn admit(self, expected: &DefaultSelection) -> Result<DefaultVersions, String> {
        if &self.before != expected || &self.after != expected {
            return Err(
                "native version producer context disagrees with admitted selection".to_owned(),
            );
        }
        self.versions.validate()?;
        Ok(self.versions)
    }
}

fn loader_part(part: Option<&std::ffi::OsStr>) -> native_default::LoaderPart {
    match part {
        Some(part) if part == "lib" => native_default::LoaderPart::Library,
        Some(part) if part == "toolchains" => native_default::LoaderPart::Toolchains,
        Some(_) => native_default::LoaderPart::Other,
        None => native_default::LoaderPart::Missing,
    }
}

fn retain_loader_path(path: &Path) -> bool {
    let mut tail = path.iter().rev();
    native_default::retain_loader_tail([
        loader_part(tail.next()),
        loader_part(tail.next()),
        loader_part(tail.next()),
    ])
}

fn prefixed_search_path(
    prefixes: &[PathBuf],
    inherited: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, String> {
    let mut paths = prefixes.to_vec();
    if let Some(inherited) = inherited {
        paths.extend(std::env::split_paths(inherited));
    }
    std::env::join_paths(paths).map_err(|error| error.to_string())
}

fn apply_proxy_runtime(command: &mut Command, context: &DefaultSelection) -> Result<(), String> {
    proxy_runtime_with(
        command,
        context,
        DefaultRuntimeInput {
            path: std::env::var_os("PATH"),
            pythonpath: std::env::var_os("PYTHONPATH"),
            loader: std::env::var_os(default_loader_name()),
        },
    )
}

const fn default_loader_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

struct DefaultRuntimeInput {
    path: Option<std::ffi::OsString>,
    pythonpath: Option<std::ffi::OsString>,
    loader: Option<std::ffi::OsString>,
}

fn proxy_runtime_with(
    command: &mut Command,
    context: &DefaultSelection,
    inherited: DefaultRuntimeInput,
) -> Result<(), String> {
    command
        .env(
            "PATH",
            prefixed_search_path(
                &[
                    context.bundle.join("bin"),
                    context.bundle.join("pyroot/bin"),
                ],
                inherited.path.as_deref(),
            )?,
        )
        .env(
            "PYTHONPATH",
            prefixed_search_path(
                &[context.bundle.join("pyroot")],
                inherited.pythonpath.as_deref(),
            )?,
        )
        .env(
            "RUSTUP_TOOLCHAIN",
            format!("nightly-2026-08-21-{}", context.target),
        );
    if let Some(paths) = inherited.loader {
        let retained = std::env::split_paths(&paths).filter(|path| retain_loader_path(path));
        command.env(
            default_loader_name(),
            std::env::join_paths(retained).map_err(|error| error.to_string())?,
        );
    }
    Ok(())
}

struct DefaultVersionProbe<'a> {
    root: &'a Path,
    evidence: &'a Path,
    context: &'a DefaultSelection,
}

impl DefaultVersionProbe<'_> {
    fn command(&self, role: &str) -> Result<Command, String> {
        let (program, runtime) = match role {
            "cargo-proxy" => (self.context.cargo_proxy.selected.clone(), false),
            "standalone-proxy" => (self.context.standalone_proxy.selected.clone(), false),
            "mise" => (self.context.mise.selected.clone(), false),
            "outer-cargo" => (self.context.outer_cargo.selected.clone(), false),
            "kani-compiler" | "cbmc" | "goto-cc" | "goto-instrument" => {
                (self.context.bundle.join("bin").join(role), true)
            }
            "linked-cargo" => (self.context.toolchain.join("bin/cargo"), true),
            "linked-rustc" => (self.context.toolchain.join("bin/rustc"), true),
            _ => return Err("unknown closed native-default version role".to_owned()),
        };
        let mut command = crate::raw::command(program);
        command.current_dir(self.root).arg("--version");
        if runtime {
            apply_proxy_runtime(&mut command, self.context)?;
        }
        Ok(command)
    }

    fn record(&self, role: &str) -> Result<DefaultVersionRecord, String> {
        let observed = capture_observed(
            &mut self.command(role)?,
            CapturePlan {
                evidence: self.evidence,
                name: role,
                limit_millis: LOCAL_LIMIT_MILLIS,
                stdout_bytes: VERSION_BYTES,
            },
        )
        .map_err(|error| error.to_string())?;
        let normal_code = observed
            .wait
            .as_ref()
            .map_err(Clone::clone)?
            .code()
            .ok_or_else(|| "native version observation was interrupted".to_owned())?;
        if normal_code != 0 {
            return Err("native version observation did not exit normally with zero".to_owned());
        }
        let bytes = bounded_bytes(&observed.stdout, VERSION_BYTES)?;
        let raw = String::from_utf8(bytes).map_err(|error| error.to_string())?;
        Ok(DefaultVersionRecord {
            role: role.to_owned(),
            raw,
            normal_code,
        })
    }
}

const DEFAULT_VERSION_ROLES: [&str; 10] = [
    "cargo-proxy",
    "standalone-proxy",
    "kani-compiler",
    "cbmc",
    "goto-cc",
    "goto-instrument",
    "mise",
    "outer-cargo",
    "linked-cargo",
    "linked-rustc",
];

impl DefaultVersions {
    fn from_records(observations: Vec<DefaultVersionRecord>) -> Result<Self, String> {
        let field = |role: &str| -> Result<String, String> {
            observations
                .iter()
                .find(|item| item.role == role)
                .and_then(|item| item.raw.lines().next())
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("missing native-default version: {role}"))
        };
        let versions = Self {
            kani: field("cargo-proxy")?,
            standalone: field("standalone-proxy")?,
            compiler: field("kani-compiler")?,
            cbmc: field("cbmc")?,
            goto_cc: field("goto-cc")?,
            goto_instrument: field("goto-instrument")?,
            mise: field("mise")?,
            outer_cargo: field("outer-cargo")?,
            linked_cargo: field("linked-cargo")?,
            linked_rustc: field("linked-rustc")?,
            observations,
        };
        versions.validate()?;
        Ok(versions)
    }

    fn validate(&self) -> Result<(), String> {
        let expected = [
            ("cargo-proxy", &self.kani),
            ("standalone-proxy", &self.standalone),
            ("kani-compiler", &self.compiler),
            ("cbmc", &self.cbmc),
            ("goto-cc", &self.goto_cc),
            ("goto-instrument", &self.goto_instrument),
            ("mise", &self.mise),
            ("outer-cargo", &self.outer_cargo),
            ("linked-cargo", &self.linked_cargo),
            ("linked-rustc", &self.linked_rustc),
        ];
        if self.observations.len() != expected.len() {
            return Err("incomplete native-default version observations".to_owned());
        }
        for (role, value) in expected {
            let matching = self
                .observations
                .iter()
                .filter(|item| item.role == role)
                .collect::<Vec<_>>();
            let [observed] = matching.as_slice() else {
                return Err("missing or duplicate native-default version role".to_owned());
            };
            if observed.normal_code != 0
                || u64::try_from(observed.raw.len()).map_err(|error| error.to_string())?
                    > VERSION_BYTES
                || observed.raw.lines().next().map(str::trim) != Some(value.as_str())
                || value.is_empty()
            {
                return Err(
                    "inconsistent native-default raw version or normal exit observation".to_owned(),
                );
            }
        }
        if self.kani == DEFAULT_CARGO_VERSION
            && self.standalone == DEFAULT_STANDALONE_VERSION
            && self.compiler == DEFAULT_COMPILER_VERSION
            && self.linked_rustc == DEFAULT_COMPILER_VERSION
            && self.cbmc == DEFAULT_CBMC_VERSION
            && self.goto_instrument == DEFAULT_CBMC_VERSION
        {
            Ok(())
        } else {
            Err(format!(
                "native-default version conflicts with the fixed release: cargo-proxy {:?}; standalone-proxy {:?}; compiler {:?}; linked-rustc {:?}; cbmc {:?}; goto-instrument {:?}",
                self.kani,
                self.standalone,
                self.compiler,
                self.linked_rustc,
                self.cbmc,
                self.goto_instrument
            ))
        }
    }
}

fn native_default_versions(
    root: &Path,
    context: &DefaultSelection,
    evidence: &Path,
) -> Result<DefaultVersions, String> {
    let rustc = bounded_bytes(&context.bundle.join("rustc-version"), VERSION_BYTES)?;
    let toolchain = bounded_bytes(
        &context.bundle.join("rust-toolchain-version"),
        VERSION_BYTES,
    )?;
    let expected = format!("nightly-2026-08-21-{}", context.target);
    if rustc != format!("{DEFAULT_COMPILER_VERSION}\n").as_bytes()
        || toolchain != expected.as_bytes()
    {
        return Err("native-default compiler or linked toolchain marker changed".to_owned());
    }
    let observation = new_evidence(evidence, EvidenceKind::SolverObservation)?;
    let probe = DefaultVersionProbe {
        root,
        evidence: &observation,
        context,
    };
    DefaultVersions::from_records(
        DEFAULT_VERSION_ROLES
            .into_iter()
            .map(|role| probe.record(role))
            .collect::<Result<_, _>>()?,
    )
}

fn default_versions(
    root: &Path,
    context: &DefaultSelection,
    evidence: &Path,
) -> Result<DefaultVersions, String> {
    let observation = new_evidence(evidence, EvidenceKind::SolverObservation)?;
    let mut command = default_observer_command(root, DefaultObserver::Versions)?;
    let output = capture(
        &mut command,
        CapturePlan {
            evidence: &observation,
            name: "default-versions",
            limit_millis: LOCAL_LIMIT_MILLIS,
            stdout_bytes: DIAGNOSTIC_BYTES,
        },
    )?;
    let observed: DefaultVersionObservation = domyjob_core::ingress::json(
        &bounded_bytes(&output, DIAGNOSTIC_BYTES)?,
        usize::try_from(DIAGNOSTIC_BYTES).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    observed.admit(context)
}

pub(crate) fn emit_default_versions(root: &Path) -> Result<(), String> {
    let evidence = default_version_evidence(root)?;
    let before = default_selection();
    let primary = match before.as_ref() {
        Ok(context) => NativeDefaultBinding::observe_using(
            || default_files(context),
            || native_default_versions(root, context, &evidence),
        ),
        Err(error) => Err(error.clone()),
    };
    let after = default_selection();
    let observed = match (before, primary, after) {
        (Ok(before), Ok((_, versions)), Ok(after)) if before == after => {
            DefaultVersionObservation {
                before,
                after,
                versions,
            }
        }
        (before, primary, after) => {
            return Err(format!(
                "native version child context refused: before {before:?}; primary {primary:?}; after {after:?}"
            ));
        }
    };
    emit_default_bytes(&serde_json::to_vec(&observed).map_err(|error| error.to_string())?)
}

fn default_version_evidence(root: &Path) -> Result<PathBuf, String> {
    evidence(root, EvidenceKind::SolverObservation)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NativeDefaultBinding {
    selection: DefaultSelection,
    files: DefaultFiles,
    versions: DefaultVersions,
}

impl NativeDefaultBinding {
    pub(crate) fn capture(root: &Path, evidence: &Path) -> Result<Self, String> {
        Self::capture_using(
            || default_context(root, evidence),
            |context| Self::observe(root, context, evidence),
            || default_context(root, evidence),
        )
    }

    fn capture_using(
        before: impl FnOnce() -> Result<DefaultSelection, String>,
        observe: impl FnOnce(&DefaultSelection) -> Result<(DefaultFiles, DefaultVersions), String>,
        after: impl FnOnce() -> Result<DefaultSelection, String>,
    ) -> Result<Self, String> {
        let before = before();
        let primary = match before.as_ref() {
            Ok(context) => observe(context),
            Err(error) => Err(error.clone()),
        };
        let after = after();
        match (before, primary, after) {
            (Ok(selection), Ok((files, versions)), Ok(after)) if selection == after => Ok(Self {
                selection,
                files,
                versions,
            }),
            (before, primary, after) => Err(format!(
                "native-default observation refused: selection {before:?}; primary {primary:?}; after {after:?}"
            )),
        }
    }

    fn observe(
        root: &Path,
        context: &DefaultSelection,
        evidence: &Path,
    ) -> Result<(DefaultFiles, DefaultVersions), String> {
        Self::observe_using(
            || default_files(context),
            || default_versions(root, context, evidence),
        )
    }

    fn observe_using(
        mut files: impl FnMut() -> Result<DefaultFiles, String>,
        versions: impl FnOnce() -> Result<DefaultVersions, String>,
    ) -> Result<(DefaultFiles, DefaultVersions), String> {
        let before = files();
        let versions = match &before {
            Ok(_) => versions(),
            Err(error) => Err(error.clone()),
        };
        let after = files();
        match (before, versions, after) {
            (Ok(before), Ok(versions), Ok(after)) if before == after => Ok((before, versions)),
            (before, versions, after) => Err(format!(
                "native-default objects or versions refused: before {before:?}; versions {versions:?}; after {after:?}"
            )),
        }
    }

    pub(crate) fn recheck(&self, root: &Path, evidence: &Path) -> Result<(), String> {
        if *self == Self::capture(root, evidence)? {
            Ok(())
        } else {
            Err("native-default selected tool or configuration changed".to_owned())
        }
    }

    pub(crate) fn target(&self) -> Result<native_default::Target, String> {
        self.selection.target()
    }

    pub(crate) const fn versions(&self) -> &DefaultVersions {
        &self.versions
    }
}
mod raw {
    #![expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "owned proof tool observations require monotonic deadlines, bounded polling sleep, and bounded context output"
    )]

    pub(super) fn clock() -> impl Fn() -> u128 {
        clock_at(std::time::Instant::now())
    }

    fn clock_at(started: std::time::Instant) -> impl Fn() -> u128 {
        move || started.elapsed().as_millis()
    }

    pub(super) fn sleep_millis(milliseconds: u64) {
        std::thread::sleep(std::time::Duration::from_millis(milliseconds));
    }

    pub(super) fn stdout() -> std::io::Stdout {
        std::io::stdout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(unix))]
    #[test]
    fn unsupported_physical_capture_refuses_before_inspecting_a_missing_path() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("not-created");
        assert_eq!(
            default_file(
                &missing,
                native_default::Target::LinuxAmd64,
                native_default::Object::Executable,
            )
            .unwrap_err(),
            "unsupported native-default physical object semantics"
        );
    }

    fn default_selection_fixture() -> DefaultSelection {
        let program = || SelectedProgram {
            selected: PathBuf::from("selected"),
            canonical: PathBuf::from("canonical"),
            links: Vec::new(),
        };
        DefaultSelection {
            target: "x86_64-unknown-linux-gnu".to_owned(),
            mise: program(),
            cargo_proxy: program(),
            standalone_proxy: program(),
            outer_cargo: program(),
            observer: PathBuf::from("observer"),
            bundle: PathBuf::from("bundle"),
            toolchain: PathBuf::from("nightly"),
            toolchain_link: SelectionLink {
                path: PathBuf::from("bundle/toolchain"),
                target: PathBuf::from("nightly"),
                device: 1,
                inode: 1,
                bytes: 0,
                mode: 0,
                owner: 0,
                group: 0,
            },
            environment: vec![("PYTHONPATH".to_owned(), None)],
        }
    }

    fn default_version_records_fixture() -> Vec<DefaultVersionRecord> {
        DEFAULT_VERSION_ROLES
            .into_iter()
            .map(|role| {
                let version = match role {
                    "cargo-proxy" => DEFAULT_CARGO_VERSION,
                    "standalone-proxy" => DEFAULT_STANDALONE_VERSION,
                    "kani-compiler" | "linked-rustc" => DEFAULT_COMPILER_VERSION,
                    "cbmc" | "goto-instrument" => DEFAULT_CBMC_VERSION,
                    _ => "observed native version",
                };
                DefaultVersionRecord {
                    role: role.to_owned(),
                    raw: format!("{version}\n"),
                    normal_code: 0,
                }
            })
            .collect()
    }

    #[test]
    fn native_default_loader_paths_match_the_pinned_proxy_suffix_filter() {
        for (path, expected) in [
            ("", true),
            ("/", true),
            ("lib", true),
            ("toolchains/lib", true),
            ("toolchains/toolchain", true),
            ("toolchains/toolchain/lib", false),
            ("prefix/toolchains/toolchain/lib/", false),
            ("toolchains/lib/lib", false),
            ("toolchains/toolchains/lib", false),
            ("toolchains//toolchain/./lib", false),
            ("toolchains/toolchain/lib/other", true),
            ("prefix/toolchains/toolchain/other/lib", true),
            ("toolchains/lib/other/lib", true),
            ("toolchains-other/toolchain/lib", true),
            ("lib/toolchains/toolchain", true),
            ("other/toolchain/lib", true),
        ] {
            let path = Path::new(path);
            let mut parts = path.iter().rev();
            let pinned = !(parts.next() == Some(std::ffi::OsStr::new("lib"))
                && parts.next().is_some()
                && parts.next() == Some(std::ffi::OsStr::new("toolchains")));
            assert_eq!(pinned, expected, "{}", path.display());
            assert_eq!(retain_loader_path(path), pinned, "{}", path.display());
        }
    }

    #[test]
    fn native_default_runtime_fields_are_command_local_and_preserve_exact_prefix_order() {
        let context = default_selection_fixture();
        let mut command = crate::raw::command("closed tool");
        proxy_runtime_with(
            &mut command,
            &context,
            DefaultRuntimeInput {
                path: Some(std::env::join_paths(["first", "second"]).unwrap()),
                pythonpath: Some(std::env::join_paths(["python-first", "python-second"]).unwrap()),
                loader: Some(
                    std::env::join_paths(["keep", "toolchains/nightly/lib", "keep-last"]).unwrap(),
                ),
            },
        )
        .unwrap();
        let environment = command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.unwrap().to_os_string()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(environment.len(), 4);
        assert_eq!(
            environment.get(std::ffi::OsStr::new("PATH")).unwrap(),
            &std::env::join_paths([
                Path::new("bundle").join("bin"),
                Path::new("bundle").join("pyroot/bin"),
                PathBuf::from("first"),
                PathBuf::from("second"),
            ])
            .unwrap()
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("PYTHONPATH")).unwrap(),
            &std::env::join_paths([
                Path::new("bundle").join("pyroot"),
                PathBuf::from("python-first"),
                PathBuf::from("python-second"),
            ])
            .unwrap()
        );
        assert_eq!(
            environment
                .get(std::ffi::OsStr::new("RUSTUP_TOOLCHAIN"))
                .unwrap(),
            "nightly-2026-08-21-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            environment
                .get(std::ffi::OsStr::new(default_loader_name()))
                .unwrap(),
            &std::env::join_paths(["keep", "keep-last"]).unwrap()
        );
        for inherited in [None, Some(std::ffi::OsString::new())] {
            let expected = if inherited.is_some() {
                std::env::join_paths(["prefix", ""]).unwrap()
            } else {
                std::env::join_paths(["prefix"]).unwrap()
            };
            assert_eq!(
                prefixed_search_path(&[PathBuf::from("prefix")], inherited.as_deref()).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn native_default_version_evidence_accepts_the_actual_manifest_parent_root() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().canonicalize().unwrap();
        let manifest = tempfile::Builder::new()
            .prefix("xtask")
            .rand_bytes(0)
            .tempdir_in(&root)
            .unwrap();
        let manifest_parent = manifest.path().join("..");
        let observed = default_version_evidence(&manifest_parent).unwrap();
        assert_eq!(observed.canonicalize().unwrap(), observed);
        assert!(observed.starts_with(root));
    }

    #[test]
    fn native_default_version_commands_preserve_admitted_alias_names() {
        let mut context = default_selection_fixture();
        for program in [
            &mut context.mise,
            &mut context.outer_cargo,
            &mut context.cargo_proxy,
            &mut context.standalone_proxy,
        ] {
            program.canonical = PathBuf::from("shared executable multiplexer");
        }
        let root = Path::new("owned root");
        let probe = DefaultVersionProbe {
            root,
            evidence: root,
            context: &context,
        };
        for (role, selection) in [
            ("mise", &context.mise),
            ("outer-cargo", &context.outer_cargo),
            ("cargo-proxy", &context.cargo_proxy),
            ("standalone-proxy", &context.standalone_proxy),
        ] {
            assert_eq!(
                probe.command(role).unwrap().get_program(),
                selection.selected.as_os_str()
            );
        }
    }

    fn assert_loader_search_paths_removed(command: &Command) {
        for loader in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
            assert_eq!(
                command
                    .get_envs()
                    .find(|(name, _)| *name == loader)
                    .map(|(_, value)| value),
                Some(None)
            );
        }
    }

    #[test]
    fn native_default_isolation_is_command_local_and_preserves_other_controls() {
        let mut command = crate::raw::command("closed native producer");
        command
            .env("LD_LIBRARY_PATH", "parent library search")
            .env("DYLD_FALLBACK_LIBRARY_PATH", "parent fallback search")
            .env("LD_PRELOAD", "unbound preload")
            .env("RUSTC_WRAPPER", "unbound wrapper")
            .env("PATH", "selected executable search");
        isolate_native_default(&mut command);
        assert_loader_search_paths_removed(&command);
        for (name, expected_value) in [
            ("LD_PRELOAD", "unbound preload"),
            ("RUSTC_WRAPPER", "unbound wrapper"),
            ("PATH", "selected executable search"),
        ] {
            assert_eq!(
                command
                    .get_envs()
                    .find(|(actual, _)| *actual == name)
                    .map(|(_, value)| value),
                Some(Some(std::ffi::OsStr::new(expected_value)))
            );
        }
    }

    #[test]
    fn native_default_observers_exclude_parent_loader_search_paths() {
        for observer in [DefaultObserver::Context, DefaultObserver::Versions] {
            let command = default_observer_command(Path::new("owned root"), observer).unwrap();
            assert_loader_search_paths_removed(&command);
        }
    }

    #[test]
    fn native_default_context_and_version_observers_share_the_exact_prefix_cwd_and_environment() {
        let root = Path::new("owned root");
        let context = default_observer_command(root, DefaultObserver::Context).unwrap();
        let versions = default_observer_command(root, DefaultObserver::Versions).unwrap();
        assert_eq!(context.get_current_dir(), versions.get_current_dir());
        assert_eq!(versions.get_current_dir(), Some(root));
        assert_eq!(context.get_program(), versions.get_program());
        assert_eq!(
            context.get_envs().collect::<Vec<_>>(),
            versions.get_envs().collect::<Vec<_>>()
        );
        let context_args = context.get_args().collect::<Vec<_>>();
        let version_args = versions.get_args().collect::<Vec<_>>();
        assert_eq!(context_args.len(), version_args.len());
        assert_eq!(
            context_args.get(..context_args.len() - 1),
            version_args.get(..version_args.len() - 1)
        );
        assert_eq!(
            context_args.last().copied(),
            Some(std::ffi::OsStr::new("__native-default-context"))
        );
        assert_eq!(
            version_args.last().copied(),
            Some(std::ffi::OsStr::new("__native-default-versions"))
        );
        let probe = DefaultVersionProbe {
            root,
            evidence: root,
            context: &default_selection_fixture(),
        };
        probe.command("arbitrary executable").unwrap_err();
    }

    #[test]
    fn native_default_raw_version_roles_require_all_unique_normal_zero_records() {
        #[derive(Clone, Copy, Debug)]
        enum Fault {
            Missing,
            Duplicate,
            Status,
            Empty,
            Oversize,
        }
        let valid = DefaultVersions::from_records(default_version_records_fixture()).unwrap();
        assert_eq!(valid.observations.len(), DEFAULT_VERSION_ROLES.len());
        for role in DEFAULT_VERSION_ROLES {
            for fault in [
                Fault::Missing,
                Fault::Duplicate,
                Fault::Status,
                Fault::Empty,
                Fault::Oversize,
            ] {
                let mut records = default_version_records_fixture();
                match fault {
                    Fault::Missing => records.retain(|item| item.role != role),
                    Fault::Duplicate => records.push(DefaultVersionRecord {
                        role: role.to_owned(),
                        raw: "duplicate\n".to_owned(),
                        normal_code: 0,
                    }),
                    Fault::Status => {
                        records
                            .iter_mut()
                            .find(|item| item.role == role)
                            .unwrap()
                            .normal_code = 1;
                    }
                    Fault::Empty => records
                        .iter_mut()
                        .find(|item| item.role == role)
                        .unwrap()
                        .raw
                        .clear(),
                    Fault::Oversize => {
                        records
                            .iter_mut()
                            .find(|item| item.role == role)
                            .unwrap()
                            .raw = "x".repeat(usize::try_from(VERSION_BYTES + 1).unwrap());
                    }
                }
                assert!(
                    DefaultVersions::from_records(records).is_err(),
                    "{role}: {fault:?}"
                );
            }
        }
    }

    #[test]
    fn native_default_version_child_refuses_either_context_change_and_raw_value_substitution() {
        #[derive(Clone, Copy, Debug)]
        enum Observation {
            Before,
            After,
            Raw,
        }
        let expected = default_selection_fixture();
        for which in [Observation::Before, Observation::After, Observation::Raw] {
            let mut observation = DefaultVersionObservation {
                before: default_selection_fixture(),
                after: default_selection_fixture(),
                versions: DefaultVersions::from_records(default_version_records_fixture()).unwrap(),
            };
            match which {
                Observation::Before => observation
                    .before
                    .environment
                    .push(("PYTHONPATH".to_owned(), Some([1; HASH_BYTES]))),
                Observation::After => observation
                    .after
                    .environment
                    .push(("PYTHONPATH".to_owned(), Some([1; HASH_BYTES]))),
                Observation::Raw => {
                    observation.versions.observations.first_mut().unwrap().raw =
                        "different raw version".to_owned();
                }
            }
            assert!(observation.admit(&expected).is_err(), "{which:?}");
        }
        DefaultVersionObservation {
            before: default_selection_fixture(),
            after: default_selection_fixture(),
            versions: DefaultVersions::from_records(default_version_records_fixture()).unwrap(),
        }
        .admit(&expected)
        .unwrap();
    }

    #[test]
    fn native_default_failed_selection_always_observes_after_without_running_versions() {
        let observed = std::cell::Cell::new(0);
        let after = std::cell::Cell::new(0);
        let error = NativeDefaultBinding::capture_using(
            || Err("selection refused".to_owned()),
            |_| {
                observed.set(observed.get() + 1);
                Err("unexpected version observation".to_owned())
            },
            || {
                after.set(after.get() + 1);
                Err("after selection refused".to_owned())
            },
        )
        .unwrap_err();
        assert_eq!(observed.get(), 0);
        assert_eq!(after.get(), 1);
        assert!(error.contains("selection refused"));
        assert!(error.contains("after selection refused"));
    }

    fn empty_default_files() -> DefaultFiles {
        DefaultFiles {
            files: Vec::new(),
            directories: Vec::new(),
            absent_directories: Vec::new(),
            roles: native_default::Roles::from_observation(0),
            members: 0,
            bytes: 0,
        }
    }

    #[test]
    fn native_default_object_or_version_failure_always_reobserves_objects() {
        for fail_before in [true, false] {
            let objects = std::cell::Cell::new(0);
            let versions = std::cell::Cell::new(0);
            let error = NativeDefaultBinding::observe_using(
                || {
                    objects.set(objects.get() + 1);
                    if objects.get() == 2 {
                        Err("after object refused".to_owned())
                    } else if fail_before {
                        Err("before object refused".to_owned())
                    } else {
                        Ok(empty_default_files())
                    }
                },
                || {
                    versions.set(versions.get() + 1);
                    Err("version refused".to_owned())
                },
            )
            .unwrap_err();
            assert_eq!(objects.get(), 2);
            assert_eq!(versions.get(), u32::from(!fail_before));
            assert!(error.contains("after object refused"));
            assert!(error.contains(if fail_before {
                "before object refused"
            } else {
                "version refused"
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_default_selection_preserves_every_literal_link_and_refuses_cycles() {
        use std::os::unix::fs::symlink;
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().canonicalize().unwrap();
        let object = root.join("actual");
        crate::raw::write(&object, b"owned object").unwrap();
        symlink("actual", root.join("second")).unwrap();
        symlink("second", root.join("first")).unwrap();
        let selected = selection_link(&root.join("first")).unwrap();
        assert_eq!(selected.selected, root.join("first"));
        assert_eq!(selected.canonical, object);
        assert_eq!(selected.links.len(), 2);
        assert_eq!(selected.links.first().unwrap().target, Path::new("second"));
        assert_eq!(selected.links.get(1).unwrap().target, Path::new("actual"));
        symlink("cycle-second", root.join("cycle-first")).unwrap();
        symlink("cycle-first", root.join("cycle-second")).unwrap();
        assert!(
            selection_link(&root.join("cycle-first"))
                .unwrap_err()
                .contains("cyclic")
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_default_replacement_directory_and_changed_content_refuse_same_path_credit() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().canonicalize().unwrap();
        let selected = tempfile::Builder::new()
            .prefix("selected")
            .rand_bytes(0)
            .tempdir_in(&root)
            .unwrap();
        let directory = selected.path().to_path_buf();
        let before = default_directory(&directory).unwrap();
        let held_directory = std::fs::File::open(&directory).unwrap();
        selected.close().unwrap();
        let replacement = tempfile::Builder::new()
            .prefix("selected")
            .rand_bytes(0)
            .tempdir_in(&root)
            .unwrap();
        assert_eq!(replacement.path(), directory);
        let after = default_directory(&directory).unwrap();
        assert!(!same_default_file(&before, &after));
        assert_ne!(before.identity.inode, after.identity.inode);
        drop(held_directory);
        let object = directory.join("support");
        crate::raw::write(&object, b"first").unwrap();
        let before_content = default_file(
            &object,
            native_default::Target::MacArm64,
            native_default::Object::Support,
        )
        .unwrap();
        crate::raw::write(&object, b"other").unwrap();
        let after_content = default_file(
            &object,
            native_default::Target::MacArm64,
            native_default::Object::Support,
        )
        .unwrap();
        assert_eq!(before_content.identity.bytes, after_content.identity.bytes);
        assert!(!same_default_file(&before_content, &after_content));
    }

    #[cfg(unix)]
    #[test]
    fn native_default_optional_pyroot_presence_is_independently_bound() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().canonicalize().unwrap();
        let path = root.join("pyroot");
        let mut absent = empty_default_files();
        default_optional_directory(&mut absent, &path).unwrap();
        assert_eq!(absent.absent_directories, std::slice::from_ref(&path));
        assert_eq!(absent.directories, []);
        crate::raw::create_dir_all(&path).unwrap();
        let mut present = empty_default_files();
        default_optional_directory(&mut present, &path).unwrap();
        assert_eq!(present.absent_directories, Vec::<PathBuf>::new());
        assert_eq!(present.directories.first().unwrap().path, path);
        assert_ne!(absent, present);
    }

    #[test]
    fn probe_limits_include_exact_bytes_and_stop_at_the_deadline() {
        assert!(output_bounds(
            VERSION_BYTES,
            DIAGNOSTIC_BYTES,
            VERSION_BYTES
        ));
        assert!(!output_bounds(VERSION_BYTES + 1, 0, VERSION_BYTES));
        assert!(!output_bounds(0, DIAGNOSTIC_BYTES + 1, VERSION_BYTES));
        assert!(deadline(
            u128::from(LOCAL_LIMIT_MILLIS - 1),
            LOCAL_LIMIT_MILLIS
        ));
        assert!(!deadline(
            u128::from(LOCAL_LIMIT_MILLIS),
            LOCAL_LIMIT_MILLIS
        ));
        assert!(!deadline(u128::MAX, LOCAL_LIMIT_MILLIS));
    }

    #[test]
    fn evidence_claims_refuse_collisions_existing_destinations_and_symbolic_parents() {
        let owned = tempfile::tempdir().unwrap();
        let parent = owned.path().canonicalize().unwrap();
        let identity = [0; EVIDENCE_ID_BYTES];
        let destination = claimed_evidence(&parent, EvidenceKind::ProofAttempt, &identity).unwrap();
        let retained = destination.join("retained");
        crate::raw::write(&retained, b"original evidence").unwrap();
        assert!(
            claimed_evidence(&parent, EvidenceKind::ProofAttempt, &identity)
                .unwrap_err()
                .starts_with("evidence identity is already claimed")
        );
        assert_eq!(bounded_bytes(&retained, 32).unwrap(), b"original evidence");
        let existing_identity = [1; EVIDENCE_ID_BYTES];
        let name = format!(
            "proof-attempt-{}",
            data_encoding::HEXLOWER.encode(&existing_identity)
        );
        crate::raw::create_dir_all(&parent.join(name)).unwrap();
        assert_eq!(
            claimed_evidence(&parent, EvidenceKind::ProofAttempt, &existing_identity).unwrap_err(),
            "claimed evidence destination already exists"
        );
        #[cfg(unix)]
        {
            let link = parent.join("symbolic-parent");
            std::os::unix::fs::symlink(&parent, &link).unwrap();
            assert_eq!(
                claimed_evidence(&link, EvidenceKind::SolverObservation, &identity).unwrap_err(),
                "evidence parent is not an owned physical directory"
            );
        }
    }
}
