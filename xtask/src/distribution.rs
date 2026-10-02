use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{Read as _, Seek as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const FILE_LIMIT: u64 = 64 * 1024 * 1024;
const OUTPUT_LIMIT: u64 = 1024 * 1024;
const RESOURCES: &[&str] = &[
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "README.md",
    "crates/domyjob/assets/icon.svg",
    "crates/domyjob/assets/icon.png",
    "crates/domyjob/assets/icon.ico",
    "crates/domyjob/assets/icon.icns",
    "crates/domyjob/assets/LICENSE",
    "crates/domyjob/assets/NOTICE",
];
const WINDOWS_VERIFY: &str = r"
$ErrorActionPreference = 'Stop'
$signature = Get-AuthenticodeSignature -LiteralPath $env:DOMYJOB_RELEASE_BINARY
$leafSha256 = $null
if ($null -ne $signature.SignerCertificate) {
  $sha256 = [System.Security.Cryptography.SHA256]::Create()
  try {
    $leafSha256 = [System.BitConverter]::ToString($sha256.ComputeHash($signature.SignerCertificate.RawData)).Replace('-', '').ToLowerInvariant()
  } finally {
    $sha256.Dispose()
  }
}
[ordered]@{
  status = $signature.Status.ToString()
  signerCertificatePresent = $null -ne $signature.SignerCertificate
  timestampCertificatePresent = $null -ne $signature.TimeStamperCertificate
  leafSha256 = $leafSha256
} | ConvertTo-Json -Compress
";

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "distribution tasks stage reviewed release files and run native packaging tools"
    )]

    use std::path::Path;

    pub(super) fn copy(source: &Path, destination: &Path) -> std::io::Result<u64> {
        std::fs::copy(source, destination)
    }

    pub(super) fn rename(source: &Path, destination: &Path) -> std::io::Result<()> {
        std::fs::rename(source, destination)
    }

    pub(super) fn create_dir_all(path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::write(path, bytes)
    }

    pub(super) fn read_to_end(
        reader: &mut impl std::io::Read,
        bytes: &mut Vec<u8>,
    ) -> std::io::Result<usize> {
        reader.read_to_end(bytes)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DistributionError {
    #[error("{0}")]
    Invalid(String),
    #[error("{operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("running {program}: {source}")]
    Start {
        program: String,
        source: std::io::Error,
    },
    #[error("{program} failed ({code:?}): {stderr}")]
    Failed {
        program: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error(transparent)]
    Json(#[from] domyjob_core::ingress::JsonError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    LinuxX86,
    LinuxArm,
    MacArm,
    MacX86,
    Windows,
}

impl Target {
    const ALL: [Self; 5] = [
        Self::LinuxX86,
        Self::LinuxArm,
        Self::MacArm,
        Self::MacX86,
        Self::Windows,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::LinuxX86 => "x86_64-unknown-linux-gnu",
            Self::LinuxArm => "aarch64-unknown-linux-gnu",
            Self::MacArm => "aarch64-apple-darwin",
            Self::MacX86 => "x86_64-apple-darwin",
            Self::Windows => "x86_64-pc-windows-msvc",
        }
    }

    fn parse(value: &str) -> Result<Self, DistributionError> {
        Self::ALL
            .into_iter()
            .find(|target| target.name() == value)
            .ok_or_else(|| {
                DistributionError::Invalid(format!("unsupported release TARGET: {value}"))
            })
    }

    const fn binary(self) -> &'static str {
        match self {
            Self::Windows => "domyjob.exe",
            Self::LinuxX86 | Self::LinuxArm | Self::MacArm | Self::MacX86 => "domyjob",
        }
    }

    fn package(self) -> String {
        format!("domyjob-{VERSION}-{}", self.name())
    }

    fn archive(self) -> String {
        format!("{}.tar.gz", self.package())
    }

    fn binary_path(self) -> PathBuf {
        PathBuf::from("target")
            .join(self.name())
            .join("release")
            .join(self.binary())
    }
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> DistributionError {
    DistributionError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>, DistributionError> {
    let file = std::fs::File::open(path).map_err(|source| io_error("reading", path, source))?;
    let mut bytes = Vec::new();
    raw::read_to_end(&mut file.take(limit.saturating_add(1)), &mut bytes)
        .map_err(|source| io_error("reading", path, source))?;
    if u64::try_from(bytes.len()).is_ok_and(|length| length <= limit) {
        Ok(bytes)
    } else {
        Err(DistributionError::Invalid(
            "release input exceeds its byte limit".to_owned(),
        ))
    }
}

fn environment(name: &str) -> Result<OsString, DistributionError> {
    std::env::var_os(name).ok_or_else(|| DistributionError::Invalid(format!("missing {name}")))
}

fn text_environment(
    environment: &impl Fn(&str) -> Result<OsString, DistributionError>,
    name: &str,
) -> Result<String, DistributionError> {
    environment(name)?
        .into_string()
        .map_err(|_non_utf8| DistributionError::Invalid(format!("{name} must be UTF-8")))
}

fn version(value: &str) -> Result<(), DistributionError> {
    if value == VERSION {
        Ok(())
    } else {
        Err(DistributionError::Invalid(format!(
            "release VERSION {value:?} does not match workspace version {VERSION}"
        )))
    }
}

fn checked_path(root: &Path, relative: &Path) -> Result<PathBuf, DistributionError> {
    let mut path = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(DistributionError::Invalid(
                "release paths must stay under the repository".to_owned(),
            ));
        };
        path.push(name);
        if components.peek().is_some() {
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|source| io_error("reading", &path, source))?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(DistributionError::Invalid(format!(
                    "expected a regular directory: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(path)
}

fn regular(path: &Path, minimum: u64, maximum: u64) -> Result<u64, DistributionError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| io_error("reading", path, source))?;
    if metadata.is_file()
        && !metadata.file_type().is_symlink()
        && (minimum..=maximum).contains(&metadata.len())
    {
        Ok(metadata.len())
    } else {
        Err(DistributionError::Invalid(format!(
            "expected a bounded regular file: {}",
            path.display()
        )))
    }
}

fn directory(path: &Path) -> Result<(), DistributionError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| io_error("reading", path, source))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(DistributionError::Invalid(format!(
            "expected a regular directory: {}",
            path.display()
        )))
    }
}

fn absent(path: &Path) -> Result<(), DistributionError> {
    match std::fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("reading", path, source)),
        Ok(_) => Err(DistributionError::Invalid(format!(
            "release output already exists: {}",
            path.display()
        ))),
    }
}

fn command(root: &Path, program: &str) -> Command {
    let mut command = crate::raw::command(program);
    command.current_dir(root).stdin(Stdio::null());
    command
}

fn captured(mut file: std::fs::File) -> Result<Vec<u8>, DistributionError> {
    let length = file
        .metadata()
        .map_err(|source| io_error("reading command output", Path::new("output"), source))?
        .len();
    if length > OUTPUT_LIMIT {
        return Err(DistributionError::Invalid(
            "release tool output exceeds 1 MiB".to_owned(),
        ));
    }
    let mut bytes = vec![
        0;
        usize::try_from(length)
            .map_err(|error| DistributionError::Invalid(error.to_string()))?
    ];
    file.rewind()
        .and_then(|()| file.read_exact(&mut bytes))
        .map_err(|source| io_error("reading command output", Path::new("output"), source))?;
    Ok(bytes)
}

fn execute(command: &mut Command) -> Result<Vec<u8>, DistributionError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let stdout = tempfile::tempfile()
        .map_err(|source| io_error("creating command output", Path::new("stdout"), source))?;
    let stderr = tempfile::tempfile()
        .map_err(|source| io_error("creating command output", Path::new("stderr"), source))?;
    command.stdout(
        stdout
            .try_clone()
            .map_err(|source| io_error("cloning command output", Path::new("stdout"), source))?,
    );
    command.stderr(
        stderr
            .try_clone()
            .map_err(|source| io_error("cloning command output", Path::new("stderr"), source))?,
    );
    let status = command
        .status()
        .map_err(|source| DistributionError::Start {
            program: program.clone(),
            source,
        })?;
    let stdout = captured(stdout)?;
    let stderr = captured(stderr)?;
    if status.success() {
        Ok(stdout)
    } else {
        Err(DistributionError::Failed {
            program,
            code: status.code(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

fn write_version(
    environment: &impl Fn(&str) -> Result<OsString, DistributionError>,
) -> Result<(), DistributionError> {
    match text_environment(environment, "EVENT_NAME")?.as_str() {
        "push" => {
            let tag = text_environment(environment, "TAG")?;
            if tag != format!("v{VERSION}")
                || text_environment(environment, "SOURCE_REF")? != format!("refs/tags/v{VERSION}")
            {
                return Err(DistributionError::Invalid(format!(
                    "the tag {tag:?} does not name version {VERSION}"
                )));
            }
        }
        "workflow_dispatch" => {
            if text_environment(environment, "SOURCE_REF")? != "refs/heads/main" {
                return Err(DistributionError::Invalid(
                    "a rehearsal must use refs/heads/main".to_owned(),
                ));
            }
        }
        event => {
            return Err(DistributionError::Invalid(format!(
                "unsupported release event: {event}"
            )));
        }
    }
    let output = PathBuf::from(environment("GITHUB_OUTPUT")?);
    regular(&output, 0, OUTPUT_LIMIT)?;
    crate::raw::append(&output, format!("version={VERSION}\n").as_bytes())
        .map_err(|source| io_error("writing", &output, source))
}

fn build(
    root: &Path,
    target: Target,
    execute: &mut impl FnMut(&mut Command) -> Result<Vec<u8>, DistributionError>,
) -> Result<(), DistributionError> {
    let mut rustup = command(root, "rustup");
    rustup.args(["target", "add", target.name()]);
    execute(&mut rustup)?;
    let mut cargo = command(root, "cargo");
    cargo.args([
        "build",
        "--locked",
        "--release",
        "--package",
        "domyjob",
        "--target",
        target.name(),
    ]);
    execute(&mut cargo)?;
    Ok(())
}

#[derive(Debug)]
struct WindowsSigningIdentity(String);

impl WindowsSigningIdentity {
    fn parse(value: String) -> Result<Self, DistributionError> {
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            Ok(Self(value))
        } else {
            Err(DistributionError::Invalid(
                "WINDOWS_SIGNING_IDENTITY_SHA256 must be exactly 64 lowercase hexadecimal digits"
                    .to_owned(),
            ))
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WindowsSignature {
    status: String,
    signer_certificate_present: bool,
    timestamp_certificate_present: bool,
    leaf_sha256: Option<String>,
}

fn windows_signature(
    bytes: &[u8],
    identity: &WindowsSigningIdentity,
) -> Result<(), DistributionError> {
    let signature: WindowsSignature = domyjob_core::ingress::json(bytes, 4096)?;
    let valid = signature.status == "Valid"
        && signature.signer_certificate_present
        && signature.timestamp_certificate_present
        && signature.leaf_sha256.as_deref() == Some(identity.0.as_str());
    if valid {
        Ok(())
    } else {
        Err(DistributionError::Invalid(
            "the binary needs a valid Authenticode signature from the configured publisher and a timestamp"
                .to_owned(),
        ))
    }
}

#[derive(Debug)]
struct VerifiedWindowsBinary {
    path: PathBuf,
    digest: String,
}

fn windows_binary_digest(path: &Path) -> Result<String, DistributionError> {
    crate::release_queue::sha256_file_bounded(path)
        .map_err(|error| DistributionError::Invalid(error.to_string()))
}

impl VerifiedWindowsBinary {
    fn unchanged(&self) -> Result<(), DistributionError> {
        if windows_binary_digest(&self.path)? == self.digest {
            Ok(())
        } else {
            Err(DistributionError::Invalid(
                "the verified Windows binary changed before distribution".to_owned(),
            ))
        }
    }

    fn stage(
        mut self,
        destination: &Path,
        copy: &impl Fn(&Path, &Path) -> std::io::Result<u64>,
    ) -> Result<Self, DistributionError> {
        self.unchanged()?;
        copy_checked(&self.path, destination, copy)?;
        self.unchanged()?;
        self.path = destination.to_path_buf();
        self.unchanged()?;
        Ok(self)
    }
}

fn verify_windows(
    root: &Path,
    target: Target,
    identity: &WindowsSigningIdentity,
    execute: &mut impl FnMut(&mut Command) -> Result<Vec<u8>, DistributionError>,
) -> Result<VerifiedWindowsBinary, DistributionError> {
    if target != Target::Windows {
        return Err(DistributionError::Invalid(
            "verify-windows requires the Windows release target".to_owned(),
        ));
    }
    let binary = checked_path(root, &target.binary_path())?;
    regular(&binary, 1, FILE_LIMIT)?;
    let digest = windows_binary_digest(&binary)?;
    let mut powershell = command(root, "pwsh");
    powershell.args(["-NoProfile", "-NonInteractive", "-Command", WINDOWS_VERIFY]);
    powershell.env("DOMYJOB_RELEASE_BINARY", &binary);
    windows_signature(&execute(&mut powershell)?, identity)?;
    let verified = VerifiedWindowsBinary {
        path: binary,
        digest,
    };
    verified.unchanged()?;
    Ok(verified)
}

fn copy_checked(
    source: &Path,
    destination: &Path,
    copy: &impl Fn(&Path, &Path) -> std::io::Result<u64>,
) -> Result<(), DistributionError> {
    let length = regular(source, 1, FILE_LIMIT)?;
    let copied =
        copy(source, destination).map_err(|source| io_error("copying", destination, source))?;
    if copied != length || regular(destination, 1, FILE_LIMIT)? != length {
        return Err(DistributionError::Invalid(format!(
            "release file changed while copying: {}",
            source.display()
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct StagedArchive {
    directory: tempfile::TempDir,
    path: PathBuf,
}

impl StagedArchive {
    fn new(dist: &Path, name: &str) -> Result<Self, DistributionError> {
        let directory = tempfile::Builder::new()
            .prefix(".release-")
            .tempdir_in(dist)
            .map_err(|source| io_error("staging", dist, source))?;
        let path = directory.path().join(name);
        Ok(Self { directory, path })
    }

    fn publish(self, destination: &Path) -> Result<(), DistributionError> {
        regular(&self.path, 1, FILE_LIMIT)?;
        absent(destination)?;
        raw::rename(&self.path, destination)
            .map_err(|source| io_error("publishing archive", destination, source))?;
        self.directory
            .close()
            .map_err(|source| io_error("cleaning staging", destination, source))
    }
}

#[derive(Debug)]
enum BundleBinary {
    Other(PathBuf),
    Windows(VerifiedWindowsBinary),
}

impl BundleBinary {
    fn prepare(
        root: &Path,
        target: Target,
        identity: Option<&WindowsSigningIdentity>,
        execute: &mut impl FnMut(&mut Command) -> Result<Vec<u8>, DistributionError>,
    ) -> Result<Self, DistributionError> {
        if target == Target::Windows {
            let identity = identity.ok_or_else(|| {
                DistributionError::Invalid("missing WINDOWS_SIGNING_IDENTITY_SHA256".to_owned())
            })?;
            verify_windows(root, target, identity, execute).map(Self::Windows)
        } else {
            let binary = checked_path(root, &target.binary_path())?;
            regular(&binary, 1, FILE_LIMIT)?;
            Ok(Self::Other(binary))
        }
    }

    fn stage(
        self,
        destination: &Path,
        copy: &impl Fn(&Path, &Path) -> std::io::Result<u64>,
    ) -> Result<Self, DistributionError> {
        match self {
            Self::Windows(verified) => verified.stage(destination, copy).map(Self::Windows),
            Self::Other(binary) => {
                copy_checked(&binary, destination, copy)?;
                Ok(Self::Other(destination.to_path_buf()))
            }
        }
    }

    fn unchanged(&self) -> Result<(), DistributionError> {
        match self {
            Self::Windows(verified) => verified.unchanged(),
            Self::Other(_) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BundleInputs<'a> {
    target: Target,
    identity: Option<&'a WindowsSigningIdentity>,
}

impl<'a> BundleInputs<'a> {
    const fn new(target: Target, identity: Option<&'a WindowsSigningIdentity>) -> Self {
        Self { target, identity }
    }
}

fn bundle(
    root: &Path,
    inputs: BundleInputs<'_>,
    execute: &mut impl FnMut(&mut Command) -> Result<Vec<u8>, DistributionError>,
    copy: &impl Fn(&Path, &Path) -> std::io::Result<u64>,
) -> Result<(), DistributionError> {
    let BundleInputs { target, identity } = inputs;
    let binary = BundleBinary::prepare(root, target, identity, execute)?;
    let sources: Vec<_> = RESOURCES
        .iter()
        .map(|name| checked_path(root, Path::new(name)))
        .collect::<Result<_, _>>()?;
    for source in &sources {
        regular(source, 1, FILE_LIMIT)?;
    }
    let dist = root.join("dist");
    raw::create_dir_all(&dist).map_err(|source| io_error("creating", &dist, source))?;
    directory(&dist)?;
    let final_archive = dist.join(target.archive());
    absent(&final_archive)?;
    let staging = StagedArchive::new(&dist, &target.archive())?;
    let package = staging.directory.path().join(target.package());
    let assets = package.join("assets");
    raw::create_dir_all(&assets).map_err(|source| io_error("creating", &assets, source))?;
    let binary = binary.stage(&package.join(target.binary()), copy)?;
    for source in &sources {
        let basename = source.file_name().ok_or_else(|| {
            DistributionError::Invalid("release resource needs a filename".to_owned())
        })?;
        let destination = if source.parent() == Some(root) {
            package.join(basename)
        } else {
            assets.join(basename)
        };
        copy_checked(source, &destination, copy)?;
    }
    let mut tar = command(staging.directory.path(), "tar");
    tar.env("COPYFILE_DISABLE", "1");
    tar.args(["--create", "--gzip", "--format=ustar", "--file"])
        .arg(&staging.path)
        .arg(target.package());
    binary.unchanged()?;
    execute(&mut tar)?;
    binary.unchanged()?;
    staging.publish(&final_archive)
}

fn archives() -> Vec<String> {
    Target::ALL.into_iter().map(Target::archive).collect()
}

fn checksum_archives(dist: &Path) -> Result<Vec<String>, DistributionError> {
    let mut names = archives();
    let packages: Vec<_> = [Target::MacArm, Target::MacX86]
        .into_iter()
        .map(|target| format!("{}.pkg", target.package()))
        .collect();
    let mut has_packages = false;
    for package in &packages {
        match std::fs::symlink_metadata(dist.join(package)) {
            Ok(_) => has_packages = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("inspecting package", &dist.join(package), source)),
        }
    }
    if has_packages {
        names.extend(packages);
    }
    Ok(names)
}

fn inventory(dist: &Path, archives: &[String]) -> Result<bool, DistributionError> {
    directory(dist)?;
    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir(dist).map_err(|source| io_error("reading", dist, source))? {
        let entry = entry.map_err(|source| io_error("reading", dist, source))?;
        let name = entry.file_name().into_string().map_err(|_non_utf8| {
            DistributionError::Invalid("release filenames must be UTF-8".to_owned())
        })?;
        names.insert(name);
    }
    let archive_names: BTreeSet<_> = archives.iter().cloned().collect();
    let all_names: BTreeSet<_> = archives
        .iter()
        .flat_map(|name| [name.clone(), format!("{name}.sha256")])
        .collect();
    let has_checksums = if names == archive_names {
        false
    } else if names == all_names {
        true
    } else {
        return Err(DistributionError::Invalid("expected the five release archives and either zero or two Mac packages, with a complete set of matching checksum files or none".to_owned()));
    };
    for name in archives {
        regular(&dist.join(name), 1, FILE_LIMIT)?;
    }
    Ok(has_checksums)
}

fn checksum(bytes: &[u8], archive: &str) -> Result<(), DistributionError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        DistributionError::Invalid(format!("checksum output must be UTF-8: {error}"))
    })?;
    let valid = text.split_once("  ").is_some_and(|(digest, name)| {
        digest.len() == 64
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && name == format!("{archive}\n")
    });
    if valid {
        Ok(())
    } else {
        Err(DistributionError::Invalid(format!(
            "expected one SHA-256 checksum for {archive}"
        )))
    }
}

fn hash_command(dist: &Path) -> Command {
    if std::env::consts::OS == "macos" {
        let mut tool = command(dist, "shasum");
        tool.args(["--algorithm", "256"]);
        tool
    } else {
        command(dist, "sha256sum")
    }
}

fn check_checksums(
    dist: &Path,
    files: &[PathBuf],
    execute: &mut impl FnMut(&mut Command) -> Result<Vec<u8>, DistributionError>,
) -> Result<(), DistributionError> {
    let mut tool = hash_command(dist);
    tool.args(["--check", "--"]).args(files);
    execute(&mut tool)?;
    Ok(())
}

fn checksum_inputs(dist: &Path, archives: &[String]) -> Result<Vec<PathBuf>, DistributionError> {
    archives
        .iter()
        .map(|name| {
            let path = dist.join(format!("{name}.sha256"));
            regular(&path, 1, 512)?;
            checksum(&read(&path, 512)?, name)?;
            Ok(path)
        })
        .collect()
}

fn checksums(
    root: &Path,
    execute: &mut impl FnMut(&mut Command) -> Result<Vec<u8>, DistributionError>,
) -> Result<(), DistributionError> {
    let dist = root.join("dist");
    let archives = checksum_archives(&dist)?;
    if inventory(&dist, &archives)? {
        check_checksums(&dist, &checksum_inputs(&dist, &archives)?, execute)?;
        inventory(&dist, &archives)?;
        return Ok(());
    }
    let staging = tempfile::Builder::new()
        .prefix(".checksums-")
        .tempdir_in(&dist)
        .map_err(|source| io_error("staging", &dist, source))?;
    let mut files = Vec::new();
    for archive in &archives {
        let mut tool = hash_command(&dist);
        tool.arg(archive);
        let output = execute(&mut tool)?;
        checksum(&output, archive)?;
        let path = staging.path().join(format!("{archive}.sha256"));
        raw::write(&path, &output).map_err(|source| io_error("writing", &path, source))?;
        files.push(path);
    }
    check_checksums(&dist, &files, execute)?;
    for file in &files {
        let name = file
            .file_name()
            .ok_or_else(|| DistributionError::Invalid("checksum needs a filename".to_owned()))?;
        raw::rename(file, &dist.join(name))
            .map_err(|source| io_error("publishing checksum", file, source))?;
    }
    staging
        .close()
        .map_err(|source| io_error("cleaning staging", &dist, source))?;
    inventory(&dist, &archives)?;
    check_checksums(&dist, &checksum_inputs(&dist, &archives)?, execute)
}

pub fn run(root: &Path, words: &[&str]) -> Result<(), DistributionError> {
    let root = root
        .canonicalize()
        .map_err(|source| io_error("reading repository", root, source))?;
    let root = crate::ci::native_command_directory(root)
        .map_err(|error| DistributionError::Invalid(error.to_string()))?;
    run_with(&root, words, &environment, &mut execute)
}

fn run_with(
    root: &Path,
    words: &[&str],
    environment: &impl Fn(&str) -> Result<OsString, DistributionError>,
    execute: &mut impl FnMut(&mut Command) -> Result<Vec<u8>, DistributionError>,
) -> Result<(), DistributionError> {
    match words {
        ["version"] => write_version(environment),
        ["build"] => {
            let target = Target::parse(&text_environment(environment, "TARGET")?)?;
            build(root, target, execute)
        }
        ["verify-windows"] => {
            let target = Target::parse(&text_environment(environment, "TARGET")?)?;
            let identity = WindowsSigningIdentity::parse(text_environment(
                environment,
                "WINDOWS_SIGNING_IDENTITY_SHA256",
            )?)?;
            verify_windows(root, target, &identity, execute).map(|_verified| ())
        }
        ["bundle"] => {
            let target = Target::parse(&text_environment(environment, "TARGET")?)?;
            version(&text_environment(environment, "VERSION")?)?;
            let identity = if target == Target::Windows {
                Some(WindowsSigningIdentity::parse(text_environment(
                    environment,
                    "WINDOWS_SIGNING_IDENTITY_SHA256",
                )?)?)
            } else {
                None
            };
            bundle(
                root,
                BundleInputs::new(target, identity.as_ref()),
                execute,
                &raw::copy,
            )
        }
        ["checksums"] => {
            version(&text_environment(environment, "VERSION")?)?;
            checksums(root, execute)
        }
        _ => Err(DistributionError::Invalid(
            "usage: release version|build|verify-windows|bundle|checksums".to_owned(),
        )),
    }
}

#[cfg(test)]
pub(crate) fn native_archive_fixture() -> Result<tempfile::TempDir, DistributionError> {
    let root = tests::fixture();
    let identity = WindowsSigningIdentity::parse(tests::WINDOWS_IDENTITY.to_owned())?;
    for target in Target::ALL {
        bundle(
            root.path(),
            BundleInputs::new(target, Some(&identity)),
            &mut |command| {
                if command.get_program() == "pwsh" {
                    serde_json::to_vec(&tests::valid_windows_signature())
                        .map_err(|error| DistributionError::Invalid(error.to_string()))
                } else {
                    execute(command)
                }
            },
            &raw::copy,
        )?;
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{
        BundleInputs, DistributionError, RESOURCES, Target, VERSION, WINDOWS_VERIFY,
        WindowsSigningIdentity, archives, build, bundle, checked_path, checksum_archives,
        checksums, command, copy_checked, execute, raw, read, regular, run_with, verify_windows,
        version, windows_signature, write_version,
    };
    use serde_json::json;

    pub(super) fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for resource in RESOURCES {
            let path = root.path().join(resource);
            raw::create_dir_all(path.parent().unwrap()).unwrap();
            raw::write(&path, resource.as_bytes()).unwrap();
        }
        for target in Target::ALL {
            let path = root.path().join(target.binary_path());
            raw::create_dir_all(path.parent().unwrap()).unwrap();
            raw::write(&path, target.binary().as_bytes()).unwrap();
        }
        root
    }

    fn dist_fixture(count: usize) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        raw::create_dir_all(&root.path().join("dist")).unwrap();
        for archive in archives().iter().take(count) {
            raw::write(&root.path().join("dist").join(archive), b"archive").unwrap();
        }
        root
    }

    fn assert_dist_entries(root: &Path, expected: usize) {
        assert_eq!(
            std::fs::read_dir(root.join("dist")).unwrap().count(),
            expected
        );
    }

    fn assert_checksums_rejected_before_commands(root: &Path) {
        assert!(matches!(
            checksums(root, &mut |_| panic!("unexpected checksum command")),
            Err(DistributionError::Invalid(_))
        ));
    }

    fn env(values: &[(&str, &str)], name: &str) -> Result<OsString, DistributionError> {
        values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| OsString::from(value))
            .ok_or_else(|| DistributionError::Invalid(format!("missing {name}")))
    }

    fn failed(program: &str) -> DistributionError {
        DistributionError::Failed {
            program: program.to_owned(),
            code: Some(7),
            stderr: "fixture command failure".to_owned(),
        }
    }

    fn arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn rejects_unknown_targets_unsafe_paths_versions_and_cli_shapes() {
        for target in Target::ALL {
            assert_eq!(Target::parse(target.name()).unwrap(), target);
            assert_eq!(target.binary() == "domyjob.exe", target == Target::Windows);
        }
        for target in ["", "../../outside", "x86_64-pc-windows-gnu", "--help"] {
            assert!(matches!(
                Target::parse(target),
                Err(DistributionError::Invalid(_))
            ));
        }
        for invalid in ["", "../outside", "different-version", "0.0.0\nother=bad"] {
            assert!(matches!(
                version(invalid),
                Err(DistributionError::Invalid(_))
            ));
        }
        version(VERSION).unwrap();
        let root = tempfile::tempdir().unwrap();
        for path in ["../outside", "/outside", "./inside"] {
            assert!(matches!(
                checked_path(root.path(), Path::new(path)),
                Err(DistributionError::Invalid(_))
            ));
        }
        for words in [vec![], vec!["version", "extra"], vec!["unknown"]] {
            assert!(matches!(
                run_with(
                    root.path(),
                    &words,
                    &|name| env(&[], name),
                    &mut |_| panic!("unexpected command")
                ),
                Err(DistributionError::Invalid(_))
            ));
        }
    }

    #[test]
    fn version_output_is_written_only_after_event_and_tag_validation() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        raw::write(&output, b"previous=value\n").unwrap();
        let output = output.to_str().unwrap();
        let wrong_tag = format!("v{VERSION}-wrong");
        let valid_tag = format!("v{VERSION}");
        for values in [
            vec![
                ("EVENT_NAME", "push"),
                ("TAG", wrong_tag.as_str()),
                ("GITHUB_OUTPUT", output),
            ],
            vec![
                ("EVENT_NAME", "push"),
                ("TAG", "v0.0.0\nother=bad"),
                ("GITHUB_OUTPUT", output),
            ],
            vec![("EVENT_NAME", "pull_request"), ("GITHUB_OUTPUT", output)],
            vec![
                ("EVENT_NAME", "workflow_dispatch"),
                ("SOURCE_REF", "refs/heads/other"),
                ("GITHUB_OUTPUT", output),
            ],
            vec![
                ("EVENT_NAME", "push"),
                ("TAG", valid_tag.as_str()),
                ("SOURCE_REF", "refs/heads/main"),
                ("GITHUB_OUTPUT", output),
            ],
        ] {
            assert!(matches!(
                write_version(&|name| env(&values, name)),
                Err(DistributionError::Invalid(_))
            ));
            assert_eq!(read(Path::new(output), 1024).unwrap(), b"previous=value\n");
        }
        let tag = format!("v{VERSION}");
        let source_ref = format!("refs/tags/v{VERSION}");
        for values in [
            vec![
                ("EVENT_NAME", "push"),
                ("TAG", tag.as_str()),
                ("SOURCE_REF", source_ref.as_str()),
                ("GITHUB_OUTPUT", output),
            ],
            vec![
                ("EVENT_NAME", "workflow_dispatch"),
                ("SOURCE_REF", "refs/heads/main"),
                ("GITHUB_OUTPUT", output),
            ],
        ] {
            write_version(&|name| env(&values, name)).unwrap();
        }
        assert_eq!(
            read(Path::new(output), 1024).unwrap(),
            format!("previous=value\nversion={VERSION}\nversion={VERSION}\n").as_bytes()
        );
        assert!(matches!(
            write_version(&|name| env(
                &[
                    ("EVENT_NAME", "workflow_dispatch"),
                    ("SOURCE_REF", "refs/heads/main"),
                    ("GITHUB_OUTPUT", "missing/output")
                ],
                name
            )),
            Err(DistributionError::Io { .. })
        ));
    }

    #[test]
    fn build_adds_the_exact_target_before_the_locked_release_build() {
        let root = tempfile::tempdir().unwrap();
        let mut seen = Vec::new();
        build(root.path(), Target::MacArm, &mut |command| {
            assert_eq!(command.get_current_dir(), Some(root.path()));
            seen.push((command.get_program().to_owned(), arguments(command)));
            Ok(Vec::new())
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                (
                    OsString::from("rustup"),
                    vec!["target", "add", "aarch64-apple-darwin"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect()
                ),
                (
                    OsString::from("cargo"),
                    vec![
                        "build",
                        "--locked",
                        "--release",
                        "--package",
                        "domyjob",
                        "--target",
                        "aarch64-apple-darwin"
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
                ),
            ]
        );
        let mut calls = 0;
        assert!(matches!(
            build(root.path(), Target::LinuxX86, &mut |_| {
                calls += 1;
                Err(failed("rustup"))
            }),
            Err(DistributionError::Failed { .. })
        ));
        assert_eq!(calls, 1);
    }

    pub(super) const WINDOWS_IDENTITY: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    pub(super) fn valid_windows_signature() -> serde_json::Value {
        json!({
            "status": "Valid",
            "signerCertificatePresent": true,
            "timestampCertificatePresent": true,
            "leafSha256": WINDOWS_IDENTITY,
        })
    }

    #[test]
    fn windows_publisher_configuration_is_required_and_canonical_before_native_commands() {
        WindowsSigningIdentity::parse(WINDOWS_IDENTITY.to_owned()).unwrap();
        let root = tempfile::tempdir().unwrap();
        for invalid in [
            String::new(),
            "a".repeat(40),
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
            WINDOWS_IDENTITY.to_ascii_uppercase(),
            format!(" {WINDOWS_IDENTITY}"),
            format!("{WINDOWS_IDENTITY}\n"),
        ] {
            assert!(matches!(
                run_with(
                    root.path(),
                    &["verify-windows"],
                    &|name| env(
                        &[
                            ("TARGET", Target::Windows.name()),
                            ("WINDOWS_SIGNING_IDENTITY_SHA256", &invalid),
                        ],
                        name,
                    ),
                    &mut |_| panic!("invalid identity reached native verification"),
                ),
                Err(DistributionError::Invalid(_))
            ));
        }
        assert!(matches!(
            run_with(
                root.path(),
                &["verify-windows"],
                &|name| env(&[("TARGET", Target::Windows.name())], name),
                &mut |_| panic!("missing identity reached native verification"),
            ),
            Err(DistributionError::Invalid(_))
        ));
    }

    #[test]
    fn windows_verification_rejects_an_os_valid_signature_from_another_publisher() {
        let identity = WindowsSigningIdentity::parse(WINDOWS_IDENTITY.to_owned()).unwrap();
        let valid = valid_windows_signature();
        windows_signature(&serde_json::to_vec(&valid).unwrap(), &identity).unwrap();
        let mut wrong_publisher = valid.clone();
        *wrong_publisher.get_mut("leafSha256").unwrap() = json!("f".repeat(64));
        assert!(matches!(
            windows_signature(&serde_json::to_vec(&wrong_publisher).unwrap(), &identity),
            Err(DistributionError::Invalid(_))
        ));
        for (key, changed) in [
            ("status", json!("NotSigned")),
            ("signerCertificatePresent", json!(false)),
            ("timestampCertificatePresent", json!(false)),
            ("signerCertificatePresent", json!("true")),
            ("leafSha256", json!("f".repeat(64))),
            ("leafSha256", json!(WINDOWS_IDENTITY.to_ascii_uppercase())),
            ("leafSha256", json!(null)),
            ("leafSha256", json!(123)),
        ] {
            let mut invalid = valid.clone();
            *invalid.get_mut(key).unwrap() = changed;
            windows_signature(&serde_json::to_vec(&invalid).unwrap(), &identity).unwrap_err();
        }
        for missing in [
            "status",
            "signerCertificatePresent",
            "timestampCertificatePresent",
            "leafSha256",
        ] {
            let mut invalid = valid.clone();
            invalid.as_object_mut().unwrap().remove(missing);
            windows_signature(&serde_json::to_vec(&invalid).unwrap(), &identity).unwrap_err();
        }
        let mut extra = valid.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("untrusted".to_owned(), json!(true));
        for invalid in [extra, json!([valid])] {
            assert!(matches!(
                windows_signature(&serde_json::to_vec(&invalid).unwrap(), &identity),
                Err(DistributionError::Json(_))
            ));
        }
        let duplicate = format!(
            r#"{{"status":"Valid","signerCertificatePresent":true,"timestampCertificatePresent":true,"leafSha256":"{}","leafSha256":"{WINDOWS_IDENTITY}"}}"#,
            "f".repeat(64),
        );
        assert!(matches!(
            windows_signature(duplicate.as_bytes(), &identity),
            Err(DistributionError::Json(_))
        ));
        assert!(matches!(
            windows_signature(b"invalid", &identity),
            Err(DistributionError::Json(_))
        ));
    }

    #[test]
    fn windows_verification_binds_the_regular_binary_to_the_configured_identity() {
        let identity = WindowsSigningIdentity::parse(WINDOWS_IDENTITY.to_owned()).unwrap();
        let root = fixture();
        let expected_binary = root.path().join(Target::Windows.binary_path());
        verify_windows(root.path(), Target::Windows, &identity, &mut |command| {
            assert_eq!(command.get_program(), "pwsh");
            assert_eq!(
                arguments(command),
                ["-NoProfile", "-NonInteractive", "-Command", WINDOWS_VERIFY]
            );
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == "DOMYJOB_RELEASE_BINARY"
                        && value == Some(expected_binary.as_os_str()))
            );
            Ok(serde_json::to_vec(&valid_windows_signature()).unwrap())
        })
        .unwrap();
        assert!(matches!(
            verify_windows(root.path(), Target::MacArm, &identity, &mut |_| panic!(
                "unexpected command"
            )),
            Err(DistributionError::Invalid(_))
        ));
        assert!(matches!(
            verify_windows(root.path(), Target::Windows, &identity, &mut |_| Err(
                failed("pwsh")
            )),
            Err(DistributionError::Failed { .. })
        ));
    }

    #[test]
    fn bundle_preserves_resources_and_windows_exe_before_publishing_archive() {
        let root = fixture();
        let target = Target::Windows;
        let identity = WindowsSigningIdentity::parse(WINDOWS_IDENTITY.to_owned()).unwrap();
        let mut tar_seen = false;
        let mut verification_seen = false;
        bundle(root.path(), BundleInputs::new(target, Some(&identity)), &mut |command| {
            if command.get_program() == "pwsh" {
                verification_seen = true;
                return Ok(serde_json::to_vec(&valid_windows_signature()).unwrap());
            }
            assert!(verification_seen);
            tar_seen = true;
            assert_eq!(command.get_program(), "tar");
            assert!(command.get_envs().any(|(name, value)| name == "COPYFILE_DISABLE" && value == Some(std::ffi::OsStr::new("1"))));
            let staging = command.get_current_dir().unwrap();
            let package = staging.join(target.package());
            assert_eq!(read(&package.join("domyjob.exe"), 1024).unwrap(), b"domyjob.exe");
            assert!(matches!(std::fs::symlink_metadata(package.join("domyjob")), Err(error) if error.kind() == std::io::ErrorKind::NotFound));
            for resource in RESOURCES {
                let source = Path::new(resource);
                let destination = if resource.starts_with("crates/") { package.join("assets").join(source.file_name().unwrap()) } else { package.join(source) };
                assert_eq!(read(&destination, 1024).unwrap(), resource.as_bytes());
            }
            assert!(matches!(std::fs::symlink_metadata(root.path().join("dist").join(target.archive())), Err(error) if error.kind() == std::io::ErrorKind::NotFound));
            raw::write(&staging.join(target.archive()), b"completed archive").unwrap();
            Ok(Vec::new())
        }, &raw::copy).unwrap();
        assert!(tar_seen);
        assert_eq!(
            read(&root.path().join("dist").join(target.archive()), 1024).unwrap(),
            b"completed archive"
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("dist")).unwrap().count(),
            1
        );
    }

    #[test]
    fn windows_bundle_cannot_skip_the_expected_publisher_check() {
        let root = fixture();
        let identity = WindowsSigningIdentity::parse(WINDOWS_IDENTITY.to_owned()).unwrap();
        bundle(
            root.path(),
            BundleInputs::new(Target::Windows, None),
            &mut |_| panic!("missing publisher policy reached native tools"),
            &raw::copy,
        )
        .unwrap_err();
        let mut wrong_publisher = valid_windows_signature();
        *wrong_publisher.get_mut("leafSha256").unwrap() = json!("f".repeat(64));
        let result = bundle(
            root.path(),
            BundleInputs::new(Target::Windows, Some(&identity)),
            &mut |command| {
                assert_eq!(command.get_program(), "pwsh");
                Ok(serde_json::to_vec(&wrong_publisher).unwrap())
            },
            &raw::copy,
        );
        assert!(matches!(result, Err(DistributionError::Invalid(_))));
        assert!(!root.path().join("dist").try_exists().unwrap());
    }

    #[test]
    fn windows_verification_rejects_changes_while_native_verification_runs() {
        let root = fixture();
        let binary = root.path().join(Target::Windows.binary_path());
        let identity = WindowsSigningIdentity::parse(WINDOWS_IDENTITY.to_owned()).unwrap();
        let result = verify_windows(root.path(), Target::Windows, &identity, &mut |_| {
            raw::write(&binary, b"modified!!!").unwrap();
            Ok(serde_json::to_vec(&valid_windows_signature()).unwrap())
        });
        assert!(matches!(result, Err(DistributionError::Invalid(_))));
    }

    #[test]
    fn windows_bundle_rejects_same_length_changes_during_copy_resources_or_tar() {
        let identity = WindowsSigningIdentity::parse(WINDOWS_IDENTITY.to_owned()).unwrap();
        for phase in ["copy", "resources", "tar"] {
            let root = fixture();
            let mut tar_seen = false;
            let result = bundle(
                root.path(),
                BundleInputs::new(Target::Windows, Some(&identity)),
                &mut |command| {
                    if command.get_program() == "pwsh" {
                        return Ok(serde_json::to_vec(&valid_windows_signature()).unwrap());
                    }
                    assert_eq!(command.get_program(), "tar");
                    tar_seen = true;
                    let staging = command.get_current_dir().unwrap();
                    let package = staging.join(Target::Windows.package());
                    if phase == "tar" {
                        raw::write(&package.join("domyjob.exe"), b"modified!!!").unwrap();
                    }
                    raw::write(&staging.join(Target::Windows.archive()), b"archive").unwrap();
                    Ok(Vec::new())
                },
                &|source, destination| {
                    let copied = raw::copy(source, destination)?;
                    if phase == "copy"
                        && source.file_name() == Some(std::ffi::OsStr::new("domyjob.exe"))
                    {
                        raw::write(destination, b"modified!!!")?;
                    }
                    if phase == "resources"
                        && source.file_name() == Some(std::ffi::OsStr::new("README.md"))
                    {
                        raw::write(
                            &destination.parent().unwrap().join("domyjob.exe"),
                            b"modified!!!",
                        )?;
                    }
                    Ok(copied)
                },
            );
            assert!(
                matches!(result, Err(DistributionError::Invalid(_))),
                "{phase}"
            );
            assert_eq!(tar_seen, phase == "tar");
            assert_dist_entries(root.path(), 0);
        }
    }

    #[test]
    fn bundle_copy_and_tar_failures_leave_no_archive_or_staging() {
        let root = fixture();
        let target = Target::LinuxX86;
        assert!(matches!(
            bundle(
                root.path(),
                BundleInputs::new(target, None),
                &mut |_| panic!("unexpected tar"),
                &|_, _| Err(crate::tests::denied())
            ),
            Err(DistributionError::Io { .. })
        ));
        assert_dist_entries(root.path(), 0);
        assert!(matches!(
            bundle(
                root.path(),
                BundleInputs::new(target, None),
                &mut |_| Err(failed("tar")),
                &raw::copy
            ),
            Err(DistributionError::Failed { .. })
        ));
        assert_dist_entries(root.path(), 0);
        assert!(matches!(
            copy_checked(
                &root.path().join("missing"),
                &root.path().join("copy"),
                &raw::copy
            ),
            Err(DistributionError::Io { .. })
        ));
        assert!(matches!(
            copy_checked(
                &root.path().join("README.md"),
                &root.path().join("copy"),
                &|_, _| Ok(0)
            ),
            Err(DistributionError::Invalid(_))
        ));
    }

    #[test]
    fn file_guards_reject_directories_empty_files_and_oversized_reads() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            regular(root.path(), 1, 1024),
            Err(DistributionError::Invalid(_))
        ));
        let path = root.path().join("file");
        raw::write(&path, b"").unwrap();
        assert!(matches!(
            regular(&path, 1, 1024),
            Err(DistributionError::Invalid(_))
        ));
        raw::write(&path, b"bounded content").unwrap();
        assert!(matches!(
            regular(&path, 1, 2),
            Err(DistributionError::Invalid(_))
        ));
        assert!(matches!(read(&path, 2), Err(DistributionError::Invalid(_))));
        let parent = root.path().join("parent-file");
        raw::write(&parent, b"not a directory").unwrap();
        assert!(matches!(
            checked_path(root.path(), Path::new("parent-file/child")),
            Err(DistributionError::Invalid(_))
        ));
    }

    #[test]
    fn missing_archives_and_incomplete_checksums_stop_before_commands() {
        let missing = dist_fixture(4);
        assert_checksums_rejected_before_commands(missing.path());
        let partial = dist_fixture(5);
        let first = archives().into_iter().next().unwrap();
        raw::write(
            &partial.path().join("dist").join(format!("{first}.sha256")),
            b"incomplete",
        )
        .unwrap();
        assert_checksums_rejected_before_commands(partial.path());
        raw::write(&partial.path().join("dist/unexpected.tar.gz"), b"unknown").unwrap();
        assert_checksums_rejected_before_commands(partial.path());
    }

    #[test]
    fn all_checksums_are_verified_before_and_after_they_are_published() {
        let root = dist_fixture(5);
        let dist = root.path().join("dist");
        let names = archives();
        let mut calls = 0;
        checksums(root.path(), &mut |command| {
            calls += 1;
            assert_eq!(command.get_current_dir(), Some(dist.as_path()));
            let args = arguments(command);
            if args.iter().any(|arg| arg == "--check") {
                let files: Vec<_> = command.get_args().skip_while(|arg| *arg != "--").skip(1).map(PathBuf::from).collect();
                assert_eq!(files.len(), 5);
                for file in &files { regular(file, 1, 512).unwrap(); }
                if calls == 6 {
                    assert!(names.iter().all(|name| matches!(std::fs::symlink_metadata(dist.join(format!("{name}.sha256"))), Err(error) if error.kind() == std::io::ErrorKind::NotFound)));
                } else {
                    assert_eq!(calls, 7);
                    assert!(files.iter().all(|file| file.parent() == Some(dist.as_path())));
                }
                Ok(Vec::new())
            } else {
                let name = args.last().unwrap();
                assert!(names.contains(name));
                Ok(format!("{}  {name}\n", "a".repeat(64)).into_bytes())
            }
        }).unwrap();
        assert_eq!(calls, 7);
        let mut checks = 0;
        checksums(root.path(), &mut |command| {
            checks += 1;
            assert!(arguments(command).iter().any(|arg| arg == "--check"));
            Ok(Vec::new())
        })
        .unwrap();
        assert_eq!(checks, 1);
    }

    #[test]
    fn installer_checksums_require_both_architectures_and_cover_all_seven_files() {
        let root = dist_fixture(5);
        let dist = root.path().join("dist");
        let arm = format!("{}.pkg", Target::MacArm.package());
        let intel = format!("{}.pkg", Target::MacX86.package());
        raw::write(&dist.join(&arm), b"signed arm installer").unwrap();
        assert!(matches!(
            checksums(root.path(), &mut |_| panic!(
                "partial package inventory reached hashing"
            )),
            Err(DistributionError::Invalid(_))
        ));
        raw::write(&dist.join(&intel), b"signed intel installer").unwrap();
        let names = checksum_archives(&dist).unwrap();
        assert_eq!(names.len(), 7);
        assert!(names.contains(&arm) && names.contains(&intel));
        let mut hashes = Vec::new();
        let mut checks = 0;
        checksums(root.path(), &mut |command| {
            let arguments = arguments(command);
            if arguments.iter().any(|word| word == "--check") {
                checks += 1;
                assert_eq!(
                    command
                        .get_args()
                        .skip_while(|word| *word != "--")
                        .skip(1)
                        .count(),
                    7
                );
                Ok(Vec::new())
            } else {
                let name = arguments.last().unwrap().clone();
                hashes.push(name.clone());
                Ok(format!("{}  {name}\n", "a".repeat(64)).into_bytes())
            }
        })
        .unwrap();
        assert_eq!(hashes, names);
        assert_eq!(checks, 2);
        assert_eq!(std::fs::read_dir(&dist).unwrap().count(), 14);
        assert_eq!(
            read(&dist.join(arm), 1024).unwrap(),
            b"signed arm installer"
        );
        assert_eq!(
            read(&dist.join(intel), 1024).unwrap(),
            b"signed intel installer"
        );
    }

    #[test]
    fn tar_bundle_preserves_an_existing_signed_installer() {
        let root = fixture();
        let dist = root.path().join("dist");
        raw::create_dir_all(&dist).unwrap();
        let installer = dist.join(format!("{}.pkg", Target::MacArm.package()));
        raw::write(&installer, b"original signed installer").unwrap();
        bundle(
            root.path(),
            BundleInputs::new(Target::MacArm, None),
            &mut |command| {
                let staging = command.get_current_dir().unwrap();
                raw::write(&staging.join(Target::MacArm.archive()), b"completed tar").unwrap();
                Ok(Vec::new())
            },
            &raw::copy,
        )
        .unwrap();
        assert_eq!(
            read(&installer, 1024).unwrap(),
            b"original signed installer"
        );
        assert_eq!(std::fs::read_dir(dist).unwrap().count(), 2);
    }

    #[test]
    fn checksum_tool_and_output_failures_never_publish_checksum_files() {
        for output in [b"".as_slice(), b"not sha256\n", b"bad  outside\n"] {
            let root = dist_fixture(5);
            assert!(matches!(
                checksums(root.path(), &mut |_| Ok(output.to_vec())),
                Err(DistributionError::Invalid(_))
            ));
            assert_eq!(
                std::fs::read_dir(root.path().join("dist")).unwrap().count(),
                5
            );
        }
        let root = dist_fixture(5);
        assert!(matches!(
            checksums(root.path(), &mut |_| Err(failed("sha256sum"))),
            Err(DistributionError::Failed { .. })
        ));
        assert_eq!(
            std::fs::read_dir(root.path().join("dist")).unwrap().count(),
            5
        );
        let mut command = command(root.path(), "domyjob-release-command-that-does-not-exist");
        assert!(matches!(
            execute(&mut command),
            Err(DistributionError::Start { .. })
        ));
    }

    #[test]
    fn checksum_verification_failure_keeps_all_generated_files_private() {
        let root = dist_fixture(5);
        let mut calls = 0;
        assert!(matches!(
            checksums(root.path(), &mut |command| {
                calls += 1;
                if arguments(command).iter().any(|arg| arg == "--check") {
                    Err(failed("checksum verification"))
                } else {
                    let name = arguments(command).pop().unwrap();
                    Ok(format!("{}  {name}\n", "a".repeat(64)).into_bytes())
                }
            }),
            Err(DistributionError::Failed { .. })
        ));
        assert_eq!(calls, 6);
        assert_eq!(
            std::fs::read_dir(root.path().join("dist")).unwrap().count(),
            5
        );
        for archive in archives() {
            raw::write(
                &root.path().join("dist").join(format!("{archive}.sha256")),
                b"malformed checksum\n",
            )
            .unwrap();
        }
        assert!(matches!(
            checksums(root.path(), &mut |_| panic!("unexpected command")),
            Err(DistributionError::Invalid(_))
        ));
    }
}
