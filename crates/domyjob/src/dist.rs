use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, Distribution};
use crate::domain::{Invalid, TargetTriple};
use crate::paths::Dirs;
use crate::protocol::VERSION;
use crate::template::{Arg, Argv, Bindings, TemplateError, Text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseKey<'a> {
    pub minisign: &'a str,
    pub ml_dsa: &'a str,
}

pub const RELEASE_KEYS: &[ReleaseKey<'static>] = &[];

pub const ML_DSA_CONTEXT: &[u8] = b"domyjob release manifest v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signatures {
    pub minisign: String,
    pub ml_dsa: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holds {
    Yes,
    No,
}
pub const OWN_TARGET: &str = env!("DOMYJOB_TARGET");

#[derive(Debug, thiserror::Error)]
pub enum DistError {
    #[error("distribution {field}: {source}")]
    Template {
        field: &'static str,
        source: TemplateError,
    },
    #[error("{program} could not start: {source}")]
    Start {
        program: String,
        source: std::io::Error,
    },
    #[error("{what} failed")]
    Failed { what: String },
    #[error(
        "this build carries no release key, so no downloaded binary can be trusted; build from source or use `setup --from`"
    )]
    NoTrustRoot,
    #[error("a release key compiled into this build is malformed")]
    TrustRoot,
    #[error(
        "the release manifest is not signed by both halves (Ed25519 and ML-DSA-65) of any key this build trusts"
    )]
    Signature,
    #[error("the release manifest is malformed: {0}")]
    Manifest(serde_json::Error),
    #[error("the release manifest is for {found}, not {wanted}")]
    WrongVersion { wanted: String, found: String },
    #[error("{found} is not newer than {current}; pass --allow-downgrade to install it anyway")]
    Downgrade { current: String, found: String },
    #[error("the release has no build for {0}")]
    NoTarget(TargetTriple),
    #[error("{what} does not match the signed digest (expected {expected}, got {actual})")]
    Digest {
        what: String,
        expected: String,
        actual: String,
    },
    #[error("{0:?} is not a version")]
    Version(String),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error(transparent)]
    Replace(crate::user_files::UserFileError),
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> DistError + use<> {
    let path = path.to_path_buf();
    move |source| DistError::Io {
        action,
        path,
        source,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    pub fn parse(text: &str) -> Result<Self, DistError> {
        let bad = || DistError::Version(text.to_owned());
        let numbers: Vec<u64> = text
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<_, _>>()
            .map_err(|_nan| bad())?;
        match numbers.as_slice() {
            [major, minor, patch] => Ok(Self(*major, *minor, *patch)),
            _ => Err(bad()),
        }
    }

    pub fn current() -> Result<Self, DistError> {
        Self::parse(VERSION)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Digests {
    pub archive_sha256: String,
    pub binary_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: String,
    pub targets: BTreeMap<TargetTriple, Digests>,
}

#[derive(Debug, Clone)]
pub struct Verified<T>(T);

impl<T> Verified<T> {
    #[must_use]
    pub const fn get(&self) -> &T {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binary {
    path: PathBuf,
    sha256: String,
}

impl Binary {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InsecureUnsigned(());

impl InsecureUnsigned {
    #[must_use]
    pub const fn acknowledged_on_the_command_line() -> Self {
        Self(())
    }
}

#[derive(Debug, Clone)]
pub enum Deliverable {
    Verified(Verified<Binary>),
    Unsigned {
        binary: Binary,
        acknowledgement: InsecureUnsigned,
    },
}

impl Deliverable {
    #[must_use]
    pub const fn binary(&self) -> &Binary {
        match self {
            Self::Verified(verified) => verified.get(),
            Self::Unsigned { binary, .. } => binary,
        }
    }
}

pub fn sha256_file(path: &Path) -> Result<String, DistError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(io("opening", path))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io("hashing", path))?;
        match buffer.get(..read) {
            Some([]) | None => break,
            Some(chunk) => hasher.update(chunk),
        }
    }
    Ok(crate::trust::hex(&hasher.finalize()))
}

pub fn running() -> Result<Verified<Binary>, DistError> {
    let path = std::env::current_exe().map_err(io("locating", Path::new("domyjob")))?;
    let sha256 = sha256_file(&path)?;
    Ok(Verified(Binary { path, sha256 }))
}

pub fn unsigned(path: &Path, acknowledgement: InsecureUnsigned) -> Result<Deliverable, DistError> {
    let sha256 = sha256_file(path)?;
    Ok(Deliverable::Unsigned {
        binary: Binary {
            path: path.to_path_buf(),
            sha256,
        },
        acknowledgement,
    })
}

fn classical(key: &ReleaseKey<'_>, bytes: &[u8], signature: &str) -> Result<Holds, DistError> {
    let public = minisign_verify::PublicKey::from_base64(key.minisign)
        .map_err(|_malformed| DistError::TrustRoot)?;
    let Ok(signature) = minisign_verify::Signature::decode(signature) else {
        return Ok(Holds::No);
    };
    Ok(match public.verify(bytes, &signature, false) {
        Ok(()) => Holds::Yes,
        Err(_forged) => Holds::No,
    })
}

fn post_quantum(key: &ReleaseKey<'_>, bytes: &[u8], signature: &str) -> Result<Holds, DistError> {
    use ml_dsa::{EncodedSignature, EncodedVerifyingKey, MlDsa65, Signature, VerifyingKey};
    let encoded = crate::trust::unhex(key.ml_dsa).ok_or(DistError::TrustRoot)?;
    let encoded = EncodedVerifyingKey::<MlDsa65>::try_from(encoded.as_slice())
        .map_err(|_wrong_length| DistError::TrustRoot)?;
    let public = VerifyingKey::<MlDsa65>::decode(&encoded);
    let Some(raw) = crate::trust::unhex(signature.trim()) else {
        return Ok(Holds::No);
    };
    let Ok(raw) = EncodedSignature::<MlDsa65>::try_from(raw.as_slice()) else {
        return Ok(Holds::No);
    };
    let Some(decoded) = Signature::<MlDsa65>::decode(&raw) else {
        return Ok(Holds::No);
    };
    Ok(
        if public.verify_with_context(bytes, ML_DSA_CONTEXT, &decoded) {
            Holds::Yes
        } else {
            Holds::No
        },
    )
}

fn verify_signatures(
    bytes: &[u8],
    signatures: &Signatures,
    keys: &[ReleaseKey<'_>],
) -> Result<(), DistError> {
    if keys.is_empty() {
        return Err(DistError::NoTrustRoot);
    }
    for key in keys {
        let both = (
            classical(key, bytes, &signatures.minisign)?,
            post_quantum(key, bytes, &signatures.ml_dsa)?,
        );
        match both {
            (Holds::Yes, Holds::Yes) => return Ok(()),
            (Holds::Yes | Holds::No, Holds::No) | (Holds::No, Holds::Yes) => {}
        }
    }
    Err(DistError::Signature)
}

pub fn verify_manifest_with(
    bytes: &[u8],
    signatures: &Signatures,
    keys: &[ReleaseKey<'_>],
) -> Result<Verified<Manifest>, DistError> {
    verify_signatures(bytes, signatures, keys)?;
    let manifest: Manifest = crate::ingress::json(bytes).map_err(DistError::Manifest)?;
    Version::parse(&manifest.version)?;
    Ok(Verified(manifest))
}

fn check_digest(what: String, expected: &str, actual: String) -> Result<(), DistError> {
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(DistError::Digest {
            what,
            expected: expected.to_owned(),
            actual,
        })
    }
}

pub fn verify_binary(
    manifest: &Verified<Manifest>,
    target: &TargetTriple,
    path: &Path,
) -> Result<Verified<Binary>, DistError> {
    let digests = manifest
        .0
        .targets
        .get(target)
        .ok_or_else(|| DistError::NoTarget(target.clone()))?;
    let sha256 = sha256_file(path)?;
    check_digest(
        path.display().to_string(),
        &digests.binary_sha256,
        sha256.clone(),
    )?;
    Ok(Verified(Binary {
        path: path.to_path_buf(),
        sha256,
    }))
}

fn run(argv: &Argv, bindings: &Bindings, field: &'static str) -> Result<bool, DistError> {
    let words = argv
        .render(bindings)
        .map_err(|source| DistError::Template { field, source })?;
    let Some(invocation) = crate::spawn::Invocation::from_words(words) else {
        return Ok(false);
    };
    let status = invocation
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|source| DistError::Start {
            program: invocation.display(),
            source,
        })?;
    Ok(status.success())
}

fn render(
    text: &Text,
    bindings: &Bindings,
    field: &'static str,
) -> Result<crate::template::Rendered, DistError> {
    text.render(bindings)
        .map_err(|source| DistError::Template { field, source })
}

fn fetch(
    distribution: &Distribution,
    url: crate::template::Rendered,
    output: &Path,
) -> Result<bool, DistError> {
    let mut partial = output.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    let bindings = Bindings::new()
        .with("url", Arg::rendered(url))
        .with("output", Arg::path(&partial));
    let fetched = run(&distribution.fetch, &bindings, "fetch")?;
    if fetched {
        crate::state_file::replace_with(&partial, output)?;
    } else {
        crate::state_file::remove_file(&partial)?;
    }
    Ok(fetched)
}

fn read_text(path: &Path) -> Result<Vec<u8>, DistError> {
    std::fs::read(path).map_err(io("reading", path))
}

pub fn fetch_manifest(
    distribution: &Distribution,
    dir: &Path,
    url: &Text,
    names: &Bindings,
) -> Result<Verified<Manifest>, DistError> {
    if RELEASE_KEYS.is_empty() {
        return Err(DistError::NoTrustRoot);
    }
    crate::state_file::private_dir(dir)?;
    let manifest_path = dir.join("manifest.json");
    let classical_path = dir.join("manifest.json.minisig");
    let quantum_path = dir.join("manifest.json.mldsa");
    let manifest_url = render(url, names, "manifest")?;
    let classical_url = crate::template::Rendered::suffixed(&manifest_url, ".minisig");
    let quantum_url = crate::template::Rendered::suffixed(&manifest_url, ".mldsa");
    let fetched = fetch(distribution, manifest_url, &manifest_path)?
        && fetch(distribution, classical_url, &classical_path)?
        && fetch(distribution, quantum_url, &quantum_path)?;
    let cached = read_text(&manifest_path).is_ok()
        && read_text(&classical_path).is_ok()
        && read_text(&quantum_path).is_ok();
    if !fetched && !cached {
        return Err(DistError::Failed {
            what: "downloading the release manifest".to_owned(),
        });
    }
    if !fetched {
        eprintln!(
            "domyjob: the release manifest could not be downloaded; using the copy verified earlier"
        );
    }
    let signatures = Signatures {
        minisign: String::from_utf8_lossy(&read_text(&classical_path)?).into_owned(),
        ml_dsa: String::from_utf8_lossy(&read_text(&quantum_path)?).into_owned(),
    };
    verify_manifest_with(&read_text(&manifest_path)?, &signatures, RELEASE_KEYS)
}

fn names(target: &TargetTriple, exe: &'static str) -> Bindings {
    Bindings::new()
        .with("version", Arg::literal(VERSION))
        .with("target", Arg::word(target))
        .with("exe", Arg::literal(exe))
}

fn download(
    distribution: &Distribution,
    dirs: &Dirs,
    target: &TargetTriple,
    exe: &'static str,
) -> Result<Verified<Binary>, DistError> {
    let bindings = names(target, exe);
    let dir = dirs.cache.join("dist").join(VERSION).join(target.as_str());
    crate::state_file::private_dir(&dir).map_err(|e| DistError::Io {
        action: "preparing",
        path: dir.clone(),
        source: std::io::Error::other(e.to_string()),
    })?;
    let _one_download_at_a_time =
        crate::lock::OsLock::exclusive(&dir.join("fetch.lock")).map_err(|e| DistError::Io {
            action: "locking",
            path: dir.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
    let manifest = fetch_manifest(distribution, &dir, &distribution.manifest, &bindings)?;
    if manifest.get().version != VERSION {
        return Err(DistError::WrongVersion {
            wanted: VERSION.to_owned(),
            found: manifest.get().version.clone(),
        });
    }
    let binary = dir.join(render(&distribution.binary, &bindings, "binary")?.as_str());
    match verify_binary(&manifest, target, &binary) {
        Ok(cached) => return Ok(cached),
        Err(DistError::Io { .. } | DistError::Digest { .. }) => {}
        Err(other) => return Err(other),
    }
    let digests = manifest
        .get()
        .targets
        .get(target)
        .ok_or_else(|| DistError::NoTarget(target.clone()))?;
    let archive = dir.join("archive");
    if !fetch(
        distribution,
        render(&distribution.archive, &bindings, "archive")?,
        &archive,
    )? {
        return Err(DistError::Failed {
            what: format!("downloading the {target} archive"),
        });
    }
    check_digest(
        archive.display().to_string(),
        &digests.archive_sha256,
        sha256_file(&archive)?,
    )?;
    let unpack = Bindings::new()
        .with("archive", Arg::path(&archive))
        .with("dir", Arg::path(&dir));
    if !run(&distribution.unpack, &unpack, "unpack")? {
        return Err(DistError::Failed {
            what: format!("unpacking {}", archive.display()),
        });
    }
    verify_binary(&manifest, target, &binary)
}

pub fn targets(os: &str, arch: &str) -> Result<Vec<TargetTriple>, DistError> {
    let texts = match os {
        "linux" => vec![
            format!("{arch}-unknown-linux-musl"),
            format!("{arch}-unknown-linux-gnu"),
        ],
        "macos" => vec![format!("{arch}-apple-darwin")],
        "windows" => vec![format!("{arch}-pc-windows-msvc")],
        other => vec![format!("{arch}-unknown-{other}")],
    };
    Ok(texts
        .into_iter()
        .map(TargetTriple::try_from)
        .collect::<Result<_, _>>()?)
}

pub fn binary_for(
    config: &Config,
    dirs: &Dirs,
    os: &str,
    arch: &str,
) -> Result<Deliverable, DistError> {
    if os == std::env::consts::OS && arch == std::env::consts::ARCH {
        return Ok(Deliverable::Verified(running()?));
    }
    let exe = if os == "windows" { ".exe" } else { "" };
    let mut last = DistError::NoTrustRoot;
    for target in targets(os, arch)? {
        let Some(distribution) = &config.distribution else {
            break;
        };
        match download(distribution, dirs, &target, exe) {
            Ok(verified) => return Ok(Deliverable::Verified(verified)),
            Err(error) => last = error,
        }
    }
    Err(last)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HighWater {
    version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downgrade {
    Refuse,
    Allow,
}

pub fn self_update(
    config: &Config,
    dirs: &Dirs,
    downgrade: Downgrade,
) -> Result<Option<Version>, DistError> {
    let Some(distribution) = &config.distribution else {
        return Ok(None);
    };
    let target = TargetTriple::try_from(OWN_TARGET.to_owned())?;
    let exe = if cfg!(windows) { ".exe" } else { "" };
    let dir = dirs.cache.join("update");
    let manifest = fetch_manifest(
        distribution,
        &dir,
        &distribution.latest,
        &names(&target, exe),
    )?;
    let found = Version::parse(&manifest.get().version)?;
    let current = Version::current()?;
    let high_water_path = dirs.state.join("highest-release.json");
    let seen = match crate::state_file::read_json::<HighWater>(&high_water_path)? {
        Some(mark) => Version::parse(&mark.version)?.max(current),
        None => current,
    };
    match (found > current && found >= seen, downgrade) {
        (true, Downgrade::Refuse | Downgrade::Allow) | (false, Downgrade::Allow) => {}
        (false, Downgrade::Refuse) if found == current => return Ok(None),
        (false, Downgrade::Refuse) => {
            return Err(DistError::Downgrade {
                current: seen.to_string(),
                found: found.to_string(),
            });
        }
    }
    let bindings = Bindings::new()
        .with("version", Arg::version(&found))
        .with("target", Arg::word(&target))
        .with("exe", Arg::literal(exe));
    let archive = dir.join("archive");
    let digests = manifest
        .get()
        .targets
        .get(&target)
        .ok_or_else(|| DistError::NoTarget(target.clone()))?;
    if !fetch(
        distribution,
        render(&distribution.archive, &bindings, "archive")?,
        &archive,
    )? {
        return Err(DistError::Failed {
            what: "downloading the update".to_owned(),
        });
    }
    check_digest(
        archive.display().to_string(),
        &digests.archive_sha256,
        sha256_file(&archive)?,
    )?;
    let unpack = Bindings::new()
        .with("archive", Arg::path(&archive))
        .with("dir", Arg::path(&dir));
    if !run(&distribution.unpack, &unpack, "unpack")? {
        return Err(DistError::Failed {
            what: "unpacking the update".to_owned(),
        });
    }
    let fresh = verify_binary(
        &manifest,
        &target,
        &dir.join(render(&distribution.binary, &bindings, "binary")?.as_str()),
    )?;
    replace_running(fresh.get().path())?;
    crate::state_file::write_json(
        &high_water_path,
        &HighWater {
            version: found.to_string(),
        },
    )?;
    Ok(Some(found))
}

fn replace_running(fresh: &Path) -> Result<(), DistError> {
    let current = std::env::current_exe().map_err(io("locating", Path::new("domyjob")))?;
    crate::user_files::replace_executable(fresh, &current).map_err(DistError::Replace)
}

impl crate::ingress::Ingress for Manifest {}
impl crate::ingress::Ingress for HighWater {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_and_parse_strictly() {
        assert!(Version::parse("0.10.0").unwrap() > Version::parse("0.9.9").unwrap());
        Version::parse("1.2").unwrap_err();
        Version::parse("1.2.x").unwrap_err();
    }

    #[test]
    fn nothing_downloaded_is_trusted_without_a_release_key() {
        let unsigned = Signatures {
            minisign: String::new(),
            ml_dsa: String::new(),
        };
        assert!(matches!(
            verify_manifest_with(b"{}", &unsigned, RELEASE_KEYS),
            Err(DistError::NoTrustRoot)
        ));
    }

    #[test]
    fn every_compiled_release_key_is_well_formed() {
        for key in RELEASE_KEYS {
            minisign_verify::PublicKey::from_base64(key.minisign).unwrap();
            let encoded = crate::trust::unhex(key.ml_dsa).unwrap();
            ml_dsa::EncodedVerifyingKey::<ml_dsa::MlDsa65>::try_from(encoded.as_slice()).unwrap();
        }
    }

    #[test]
    fn targets_reject_words_that_are_not_target_triples() {
        assert_eq!(
            targets("linux", "x86_64")
                .unwrap()
                .first()
                .unwrap()
                .as_str(),
            "x86_64-unknown-linux-musl"
        );
        targets("linux", "x86_64 -o /tmp/owned").unwrap_err();
    }

    #[test]
    fn digests_are_compared_exactly() {
        check_digest("a".into(), "ABCD", "abcd".into()).unwrap();
        assert!(matches!(
            check_digest("a".into(), "abcd", "abce".into()),
            Err(DistError::Digest { .. })
        ));
    }
}
