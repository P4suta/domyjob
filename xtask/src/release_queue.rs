use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LIMIT: u64 = 64 * 1024 * 1024;
const JSON_LIMIT: u64 = 1024 * 1024;
const DEADLINE: u64 = 7 * 24 * 60 * 60;
const REPOSITORY: &str = "P4suta/domyjob";
const MANIFEST: &str = "manifest.json";
pub(crate) const MAC_TARGETS: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
pub(crate) const TARGETS: [&str; 5] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
];
const RESOURCES: [&str; 9] = [
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "README.md",
    "assets/icon.svg",
    "assets/icon.png",
    "assets/icon.ico",
    "assets/icon.icns",
    "assets/LICENSE",
    "assets/NOTICE",
];

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueError {
    #[error("invalid release handoff: {0}")]
    Invalid(String),
    #[error("{label}: {source}")]
    Io {
        label: &'static str,
        source: std::io::Error,
    },
    #[error("release handoff JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{operation}; extraction cleanup also failed: {cleanup}")]
    Cleanup {
        operation: Box<Self>,
        cleanup: Box<Self>,
    },
}

fn invalid(label: &str) -> QueueError {
    QueueError::Invalid(label.to_owned())
}

const fn io_error(label: &'static str, source: std::io::Error) -> QueueError {
    QueueError::Io { label, source }
}

fn require(condition: bool, label: &str) -> Result<(), QueueError> {
    if condition {
        Ok(())
    } else {
        Err(invalid(label))
    }
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourceClaims {
    pub(crate) source_sha: String,
    pub(crate) origin_run_id: u64,
    pub(crate) run_attempt: u64,
    pub(crate) source_ref: String,
    pub(crate) event: String,
    pub(crate) version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "SourceClaims", into = "SourceClaims")]
pub(crate) struct Source {
    sha: String,
    origin_run_id: u64,
    run_attempt: u64,
    reference: String,
    event: String,
    version: String,
}

impl Source {
    pub(crate) fn new(claims: SourceClaims) -> Result<Self, QueueError> {
        Self::try_from(claims)
    }

    pub(crate) fn source_sha(&self) -> &str {
        &self.sha
    }
    pub(crate) const fn origin_run_id(&self) -> u64 {
        self.origin_run_id
    }
    pub(crate) const fn run_attempt(&self) -> u64 {
        self.run_attempt
    }
    pub(crate) fn source_ref(&self) -> &str {
        &self.reference
    }
    pub(crate) fn event(&self) -> &str {
        &self.event
    }
    pub(crate) fn version(&self) -> &str {
        &self.version
    }
    pub(crate) fn publish_tag(&self) -> Option<&str> {
        self.reference.strip_prefix("refs/tags/")
    }
}

impl TryFrom<SourceClaims> for Source {
    type Error = QueueError;

    fn try_from(claims: SourceClaims) -> Result<Self, Self::Error> {
        require(
            hex(&claims.source_sha, 40),
            "source SHA must be 40 lowercase hex characters",
        )?;
        require(
            claims.origin_run_id > 0 && claims.run_attempt > 0,
            "run and attempt must be positive",
        )?;
        let version = semver::Version::parse(&claims.version)
            .map_err(|error| invalid(&format!("version must be valid SemVer: {error}")))?;
        require(
            version.to_string() == claims.version && claims.version.len() <= 128,
            "version must use canonical bounded SemVer",
        )?;
        let tag_ref = format!("refs/tags/v{}", claims.version);
        require(
            matches!(claims.event.as_str(), "push" | "workflow_dispatch")
                && match claims.event.as_str() {
                    "push" => claims.source_ref == tag_ref,
                    "workflow_dispatch" => claims.source_ref == "refs/heads/main",
                    _ => false,
                },
            "source event and ref do not identify a tag release or main rehearsal",
        )?;
        Ok(Self {
            sha: claims.source_sha,
            origin_run_id: claims.origin_run_id,
            run_attempt: claims.run_attempt,
            reference: claims.source_ref,
            event: claims.event,
            version: claims.version,
        })
    }
}

impl From<Source> for SourceClaims {
    fn from(source: Source) -> Self {
        Self {
            source_sha: source.sha,
            origin_run_id: source.origin_run_id,
            run_attempt: source.run_attempt,
            source_ref: source.reference,
            event: source.event,
            version: source.version,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingClaims {
    pub(crate) source: Source,
    pub(crate) target: String,
    pub(crate) submission_id: String,
    pub(crate) binary_sha256: String,
    pub(crate) submission_zip_sha256: String,
    pub(crate) signing_identity_sha1: String,
    pub(crate) cdhash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "PendingInput", into = "PendingInput")]
pub(crate) struct PendingNotarization {
    source: Source,
    target: String,
    submission_id: String,
    binary_sha256: String,
    submission_zip_sha256: String,
    signing_identity_sha1: String,
    cdhash: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingInput {
    schema_version: u8,
    #[serde(flatten)]
    claims: PendingClaims,
}

fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
        && value != "00000000-0000-0000-0000-000000000000"
}

impl PendingNotarization {
    pub(crate) fn new(claims: PendingClaims) -> Result<Self, QueueError> {
        require(
            MAC_TARGETS.contains(&claims.target.as_str()),
            "unknown notarization target",
        )?;
        require(
            uuid(&claims.submission_id),
            "submission ID must be a canonical nonzero UUID",
        )?;
        require(
            hex(&claims.binary_sha256, 64) && hex(&claims.submission_zip_sha256, 64),
            "binary and submission ZIP hashes must be SHA-256",
        )?;
        require(
            hex(&claims.signing_identity_sha1, 40) && hex(&claims.cdhash, 40),
            "signing identity and CDHash must be 40 lowercase hex characters",
        )?;
        Ok(Self {
            source: claims.source,
            target: claims.target,
            submission_id: claims.submission_id,
            binary_sha256: claims.binary_sha256,
            submission_zip_sha256: claims.submission_zip_sha256,
            signing_identity_sha1: claims.signing_identity_sha1,
            cdhash: claims.cdhash,
        })
    }

    pub(crate) fn load(path: &Path, source: &Source, target: &str) -> Result<Self, QueueError> {
        let receipt: Self = decode(&read_bounded(path, JSON_LIMIT)?)?;
        require(
            receipt.source == *source && receipt.target == target,
            "notarization receipt source or target mismatch",
        )?;
        Ok(receipt)
    }

    pub(crate) fn store(&self, path: &Path) -> Result<(), QueueError> {
        store_json(path, self)
    }

    pub(crate) const fn source(&self) -> &Source {
        &self.source
    }
    pub(crate) fn target(&self) -> &str {
        &self.target
    }
    pub(crate) fn submission_id(&self) -> &str {
        &self.submission_id
    }
    pub(crate) fn binary_sha256(&self) -> &str {
        &self.binary_sha256
    }
    pub(crate) fn signing_identity_sha1(&self) -> &str {
        &self.signing_identity_sha1
    }
    pub(crate) fn cdhash(&self) -> &str {
        &self.cdhash
    }
}

impl TryFrom<PendingInput> for PendingNotarization {
    type Error = QueueError;

    fn try_from(input: PendingInput) -> Result<Self, Self::Error> {
        require(
            input.schema_version == 1,
            "unknown notarization receipt schema",
        )?;
        Self::new(input.claims)
    }
}

impl From<PendingNotarization> for PendingInput {
    fn from(receipt: PendingNotarization) -> Self {
        Self {
            schema_version: 1,
            claims: PendingClaims {
                source: receipt.source,
                target: receipt.target,
                submission_id: receipt.submission_id,
                binary_sha256: receipt.binary_sha256,
                submission_zip_sha256: receipt.submission_zip_sha256,
                signing_identity_sha1: receipt.signing_identity_sha1,
                cdhash: receipt.cdhash,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotaryState {
    InProgress,
    Accepted,
    Invalid,
    Expired,
}

impl NotaryState {
    pub(crate) fn parse(value: &str) -> Result<Self, QueueError> {
        match value {
            "In Progress" => Ok(Self::InProgress),
            "Accepted" => Ok(Self::Accepted),
            "Invalid" => Ok(Self::Invalid),
            _ => Err(invalid("unknown Apple notarization state")),
        }
    }

    pub(crate) fn with_deadline(self, origin_unix: u64, now_unix: u64) -> Result<Self, QueueError> {
        require(now_unix >= origin_unix, "queue origin lies in the future")?;
        let expires = origin_unix
            .checked_add(DEADLINE)
            .ok_or_else(|| invalid("queue deadline overflow"))?;
        Ok(if now_unix >= expires {
            Self::Expired
        } else {
            self
        })
    }
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, QueueError> {
    require(
        u64::try_from(bytes.len()).is_ok_and(|length| length <= JSON_LIMIT),
        "JSON exceeds its bound",
    )?;
    raw::json(bytes).map_err(QueueError::Json)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, QueueError> {
    regular(path, limit)?;
    let mut input = raw::open(path).map_err(|source| io_error("opening release input", source))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = input
            .read(&mut chunk)
            .map_err(|source| io_error("reading release input", source))?;
        if count == 0 {
            break;
        }
        let portion = chunk
            .get(..count)
            .ok_or_else(|| invalid("input read exceeds its buffer"))?;
        bytes.extend_from_slice(portion);
        require(
            u64::try_from(bytes.len()).is_ok_and(|length| length <= limit),
            "release input exceeds its bound",
        )?;
    }
    Ok(bytes)
}

fn regular(path: &Path, limit: u64) -> Result<(), QueueError> {
    let metadata =
        raw::metadata(path).map_err(|source| io_error("inspecting release input", source))?;
    require(
        metadata.file_type().is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() > 0
            && metadata.len() <= limit,
        "expected a bounded regular file without symlinks",
    )
}

pub(crate) fn sha256_file_bounded(path: &Path) -> Result<String, QueueError> {
    Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(read_bounded(path, LIMIT)?)))
}

#[derive(Debug)]
struct StagedReceipt {
    file: tempfile::NamedTempFile,
}

fn store_json(path: &Path, value: &impl Serialize) -> Result<(), QueueError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("receipt path needs a parent"))?;
    require(
        raw::metadata(parent)
            .map_err(|source| io_error("inspecting receipt parent", source))?
            .file_type()
            .is_dir(),
        "receipt parent must be a directory without symlinks",
    )?;
    match raw::metadata(path) {
        Ok(_) => return Err(invalid("refusing to replace an existing receipt")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("inspecting receipt destination", source)),
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    require(
        u64::try_from(bytes.len()).is_ok_and(|length| length <= JSON_LIMIT),
        "receipt exceeds its bound",
    )?;
    let mut staging = StagedReceipt {
        file: raw::private_file(parent)
            .map_err(|source| io_error("staging release receipt", source))?,
    };
    staging
        .file
        .write_all(&bytes)
        .map_err(|source| io_error("writing release receipt", source))?;
    staging
        .file
        .as_file()
        .sync_all()
        .map_err(|source| io_error("syncing release receipt", source))?;
    raw::hard_link(staging.file.path(), path)
        .map_err(|source| io_error("publishing release receipt without replacement", source))?;
    staging
        .file
        .close()
        .map_err(|source| io_error("cleaning staged release receipt", source))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveRecord {
    target: String,
    archive: String,
    sha256: String,
}

fn archive_name(source: &Source, target: &str) -> String {
    format!("domyjob-{}-{target}.tar.gz", source.version())
}

fn receipt_name(target: &str) -> String {
    format!("{target}.json")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "ManifestInput", into = "ManifestInput")]
pub(crate) struct PendingManifest {
    source: Source,
    archives: Vec<ArchiveRecord>,
    notarizations: Vec<PendingNotarization>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestInput {
    schema_version: u8,
    repository: String,
    source: Source,
    archives: Vec<ArchiveRecord>,
    notarizations: Vec<PendingNotarization>,
}

impl PendingManifest {
    pub(crate) fn load(path: &Path) -> Result<Self, QueueError> {
        decode(&read_bounded(path, JSON_LIMIT)?)
    }

    pub(crate) const fn source(&self) -> &Source {
        &self.source
    }
}

impl TryFrom<ManifestInput> for PendingManifest {
    type Error = QueueError;

    fn try_from(input: ManifestInput) -> Result<Self, Self::Error> {
        require(
            input.schema_version == 1 && input.repository == REPOSITORY,
            "unknown manifest schema or repository",
        )?;
        let targets: BTreeSet<_> = input
            .archives
            .iter()
            .map(|item| item.target.as_str())
            .collect();
        require(
            input.archives.len() == TARGETS.len() && targets == BTreeSet::from(TARGETS),
            "manifest needs exactly five distinct archive targets",
        )?;
        for archive in &input.archives {
            require(
                archive.archive == archive_name(&input.source, &archive.target)
                    && hex(&archive.sha256, 64),
                "manifest archive name or digest mismatch",
            )?;
        }
        let mac_targets: BTreeSet<_> = input
            .notarizations
            .iter()
            .map(PendingNotarization::target)
            .collect();
        require(
            input.notarizations.len() == MAC_TARGETS.len()
                && mac_targets == BTreeSet::from(MAC_TARGETS),
            "manifest needs exactly two distinct Mac receipts",
        )?;
        require(
            input
                .notarizations
                .iter()
                .all(|receipt| receipt.source() == &input.source),
            "manifest notarization source mismatch",
        )?;
        Ok(Self {
            source: input.source,
            archives: input.archives,
            notarizations: input.notarizations,
        })
    }
}

impl From<PendingManifest> for ManifestInput {
    fn from(manifest: PendingManifest) -> Self {
        Self {
            schema_version: 1,
            repository: REPOSITORY.to_owned(),
            source: manifest.source,
            archives: manifest.archives,
            notarizations: manifest.notarizations,
        }
    }
}

fn inventory(directory: &Path, expected: &BTreeMap<String, bool>) -> Result<(), QueueError> {
    let metadata = raw::metadata(directory)
        .map_err(|source| io_error("inspecting handoff directory", source))?;
    require(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "handoff directory must not be a symlink",
    )?;
    let mut observed = BTreeSet::new();
    for item in
        raw::read_dir(directory).map_err(|source| io_error("listing handoff files", source))?
    {
        let item = item.map_err(|source| io_error("reading handoff entry", source))?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_name| invalid("handoff file name is not UTF-8"))?;
        let kind = item
            .file_type()
            .map_err(|source| io_error("inspecting handoff entry", source))?;
        require(
            expected.get(&name).is_some_and(|is_directory| {
                if *is_directory {
                    kind.is_dir() && !kind.is_symlink()
                } else {
                    kind.is_file() && !kind.is_symlink()
                }
            }),
            "unexpected handoff file, alias, or link",
        )?;
        observed.insert(name);
    }
    require(
        observed == expected.keys().cloned().collect(),
        "handoff inventory is incomplete",
    )
}

fn handoff_inventory(directory: &Path, source: &Source, manifest: bool) -> Result<(), QueueError> {
    let mut expected = BTreeMap::from([("notarization".to_owned(), true)]);
    for target in TARGETS {
        let archive = archive_name(source, target);
        expected.insert(format!("{archive}.sha256"), false);
        expected.insert(archive, false);
    }
    if manifest {
        expected.insert(MANIFEST.to_owned(), false);
    }
    inventory(directory, &expected)?;
    inventory(
        &directory.join("notarization"),
        &MAC_TARGETS
            .map(|target| (receipt_name(target), false))
            .into_iter()
            .collect(),
    )
}

fn inspect_archive(
    directory: &Path,
    source: &Source,
    target: &str,
) -> Result<ArchiveRecord, QueueError> {
    let archive = archive_name(source, target);
    let sha256 = sha256_file_bounded(&directory.join(&archive))?;
    let checksum = read_bounded(&directory.join(format!("{archive}.sha256")), 512)?;
    require(
        checksum == format!("{sha256}  {archive}\n").as_bytes(),
        "archive checksum mismatch or unsafe checksum entry",
    )?;
    Ok(ArchiveRecord {
        target: target.to_owned(),
        archive,
        sha256,
    })
}

pub(crate) fn create_handoff(
    directory: &Path,
    source: &Source,
) -> Result<PendingManifest, QueueError> {
    handoff_inventory(directory, source, false)?;
    let archives = TARGETS
        .into_iter()
        .map(|target| inspect_archive(directory, source, target))
        .collect::<Result<Vec<_>, _>>()?;
    let notarizations = MAC_TARGETS
        .into_iter()
        .map(|target| {
            PendingNotarization::load(
                &directory.join("notarization").join(receipt_name(target)),
                source,
                target,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let manifest = PendingManifest::try_from(ManifestInput {
        schema_version: 1,
        repository: REPOSITORY.to_owned(),
        source: source.clone(),
        archives,
        notarizations,
    })?;
    store_json(&directory.join(MANIFEST), &manifest)?;
    Ok(manifest)
}

#[derive(Debug)]
pub(crate) struct Handoff {
    directory: PathBuf,
    manifest: PendingManifest,
    archives: Vec<(ArchiveRecord, PathBuf)>,
}

impl Handoff {
    pub(crate) fn inspect(directory: &Path, source: &Source) -> Result<Self, QueueError> {
        handoff_inventory(directory, source, true)?;
        let manifest = PendingManifest::load(&directory.join(MANIFEST))?;
        require(
            manifest.source() == source,
            "manifest source differs from the verified origin",
        )?;
        for archive in &manifest.archives {
            require(
                inspect_archive(directory, source, &archive.target)? == *archive,
                "archive differs from the manifest",
            )?;
        }
        for receipt in &manifest.notarizations {
            let actual = PendingNotarization::load(
                &directory
                    .join("notarization")
                    .join(receipt_name(receipt.target())),
                source,
                receipt.target(),
            )?;
            require(actual == *receipt, "receipt differs from the manifest")?;
        }
        let archives = manifest
            .archives
            .iter()
            .map(|archive| (archive.clone(), directory.join(&archive.archive)))
            .collect();
        Ok(Self {
            directory: directory.to_path_buf(),
            manifest,
            archives,
        })
    }

    pub(crate) const fn source(&self) -> &Source {
        self.manifest.source()
    }
    pub(crate) fn dir(&self) -> &Path {
        &self.directory
    }
    pub(crate) fn archives(&self) -> impl Iterator<Item = (&str, &Path, &str)> {
        self.archives.iter().map(|(archive, path)| {
            (
                archive.archive.as_str(),
                path.as_path(),
                archive.sha256.as_str(),
            )
        })
    }
    pub(crate) fn archive(&self, target: &str) -> Result<&Path, QueueError> {
        self.archives
            .iter()
            .find(|(archive, _)| archive.target == target)
            .map(|(_, path)| path.as_path())
            .ok_or_else(|| invalid("unknown archive target"))
    }
    pub(crate) fn receipt(&self, target: &str) -> Result<&PendingNotarization, QueueError> {
        self.manifest
            .notarizations
            .iter()
            .find(|receipt| receipt.target() == target)
            .ok_or_else(|| invalid("unknown notarization target"))
    }
}

#[derive(Debug)]
pub(crate) struct ExtractedBundle {
    directory: tempfile::TempDir,
    source: Source,
    binaries: BTreeMap<String, PathBuf>,
    receipts: Vec<PendingNotarization>,
}

impl ExtractedBundle {
    pub(crate) fn extract(handoff: &Handoff) -> Result<Self, QueueError> {
        let directory = raw::temporary_directory()
            .map_err(|error| io_error("creating private extraction directory", error))?;
        Self::extract_into(handoff, directory)
    }

    fn extract_into(handoff: &Handoff, directory: tempfile::TempDir) -> Result<Self, QueueError> {
        let mut extracted = Self {
            directory,
            source: handoff.source().clone(),
            binaries: BTreeMap::new(),
            receipts: handoff.manifest.notarizations.clone(),
        };
        match extracted.populate(handoff) {
            Ok(()) => Ok(extracted),
            Err(operation) => match extracted.close() {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(QueueError::Cleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            },
        }
    }

    fn populate(&mut self, handoff: &Handoff) -> Result<(), QueueError> {
        for record in &handoff.manifest.archives {
            let path = handoff.archive(&record.target)?;
            require(
                sha256_file_bounded(path)? == record.sha256,
                "archive changed before extraction",
            )?;
            let payload = extract_archive(path, handoff.source(), &record.target)?;
            require(
                sha256_file_bounded(path)? == record.sha256,
                "archive changed during extraction",
            )?;
            if MAC_TARGETS.contains(&record.target.as_str()) {
                let receipt = handoff.receipt(&record.target)?;
                require(
                    data_encoding::HEXLOWER.encode(&Sha256::digest(&payload))
                        == receipt.binary_sha256(),
                    "extracted binary differs from its submission receipt",
                )?;
                let destination = self.directory.path().join(&record.target);
                let mut file = raw::create_new(&destination)
                    .map_err(|source| io_error("creating private extracted binary", source))?;
                file.write_all(&payload)
                    .map_err(|source| io_error("writing private extracted binary", source))?;
                self.binaries.insert(record.target.clone(), destination);
            }
        }
        Ok(())
    }

    pub(crate) fn binary(&self, target: &str) -> Result<&Path, QueueError> {
        self.binaries
            .get(target)
            .map(PathBuf::as_path)
            .ok_or_else(|| invalid("unknown extracted Mac target"))
    }
    pub(crate) const fn source(&self) -> &Source {
        &self.source
    }
    pub(crate) fn receipt(&self, target: &str) -> Result<&PendingNotarization, QueueError> {
        self.receipts
            .iter()
            .find(|receipt| receipt.target() == target)
            .ok_or_else(|| invalid("unknown extracted receipt target"))
    }
    pub(crate) fn close(self) -> Result<(), QueueError> {
        self.directory
            .close()
            .map_err(|source| io_error("removing private extracted binaries", source))
    }
}

fn entry_path(bytes: &[u8]) -> Result<String, QueueError> {
    let name = std::str::from_utf8(bytes).map_err(|_error| invalid("archive name is not UTF-8"))?;
    let name = name.strip_suffix('/').unwrap_or(name);
    require(
        !name.is_empty()
            && !name.contains('\\')
            && name.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "unsafe archive path",
    )?;
    Ok(name.to_owned())
}

fn archive_inventory(source: &Source, target: &str) -> (BTreeMap<String, bool>, String) {
    let package = format!("domyjob-{}-{target}", source.version());
    let binary = if target == "x86_64-pc-windows-msvc" {
        "domyjob.exe"
    } else {
        "domyjob"
    };
    let mut expected: BTreeMap<_, _> = RESOURCES
        .into_iter()
        .map(|name| (format!("{package}/{name}"), false))
        .collect();
    let binary_name = format!("{package}/{binary}");
    expected.insert(binary_name.clone(), false);
    expected.insert(package.clone(), true);
    expected.insert(format!("{package}/assets"), true);
    (expected, binary_name)
}

fn extract_archive(path: &Path, source: &Source, target: &str) -> Result<Vec<u8>, QueueError> {
    let (expected, binary_name) = archive_inventory(source, target);
    let file = raw::open(path).map_err(|error| io_error("opening release archive", error))?;
    let reader = flate2::read::MultiGzDecoder::new(file).take(LIMIT + JSON_LIMIT);
    let mut archive = tar::Archive::new(reader);
    let mut observed = BTreeSet::new();
    let mut total = 0_u64;
    let mut payload = Vec::new();
    for entry in archive
        .entries()
        .map_err(|error| io_error("reading archive entries", error))?
        .raw(true)
    {
        let mut entry = entry.map_err(|error| io_error("reading archive entry", error))?;
        require(
            observed.len() < expected.len(),
            "archive contains too many entries",
        )?;
        let name = entry_path(&entry.path_bytes())?;
        let kind = entry.header().entry_type();
        require(
            expected.get(&name).is_some_and(|is_directory| {
                if *is_directory {
                    kind.is_dir() && !kind.is_symlink()
                } else {
                    kind.is_file() && !kind.is_symlink()
                }
            }),
            "archive entry is unexpected, aliased, or linked",
        )?;
        require(
            observed.insert(name.clone()),
            "archive contains duplicate paths",
        )?;
        let size = entry.size();
        require(
            size <= LIMIT && if kind.is_dir() { size == 0 } else { size > 0 },
            "archive entry has an invalid size",
        )?;
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("archive payload size overflow"))?;
        require(total <= LIMIT, "archive payload exceeds its bound")?;
        if name == binary_name {
            payload = read_payload(&mut entry, size)?;
        }
    }
    require(
        observed == expected.into_keys().collect(),
        "archive inventory is incomplete",
    )?;
    let mut trailing = archive.into_inner();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = trailing
            .read(&mut chunk)
            .map_err(|error| io_error("checking archive trailer", error))?;
        if count == 0 {
            break;
        }
        require(
            chunk
                .get(..count)
                .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0)),
            "archive has nonzero trailing data",
        )?;
    }
    require(
        trailing.limit() > 0,
        "decompressed archive exceeds its bound",
    )?;
    Ok(payload)
}

fn read_payload(reader: &mut impl Read, size: u64) -> Result<Vec<u8>, QueueError> {
    let length =
        usize::try_from(size).map_err(|_error| invalid("archive payload cannot fit memory"))?;
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .map_err(|source| io_error("reading archive binary", source))?;
    Ok(payload)
}

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "release queue owns bounded public receipt inputs and staged artifact files"
    )]

    use std::io;
    use std::path::Path;

    pub(super) fn open(path: &Path) -> io::Result<std::fs::File> {
        std::fs::File::open(path)
    }
    pub(super) fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(path)
    }
    pub(super) fn json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
        serde_json::from_slice(bytes)
    }
    pub(super) fn private_file(parent: &Path) -> io::Result<tempfile::NamedTempFile> {
        tempfile::Builder::new()
            .prefix(".notarization-")
            .tempfile_in(parent)
    }
    pub(super) fn hard_link(source: &Path, destination: &Path) -> io::Result<()> {
        std::fs::hard_link(source, destination)
    }
    pub(super) fn read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(path)
    }
    pub(super) fn temporary_directory() -> io::Result<tempfile::TempDir> {
        tempfile::Builder::new().prefix(".release-queue-").tempdir()
    }
    pub(super) fn create_new(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
    #[cfg(test)]
    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
pub(crate) fn handoff_fixture() -> (tempfile::TempDir, Handoff) {
    tests::build_handoff_fixture()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;

    use super::{
        DEADLINE, ExtractedBundle, Handoff, MAC_TARGETS, NotaryState, PendingClaims,
        PendingManifest, PendingNotarization, QueueError, RESOURCES, Source, SourceClaims, TARGETS,
        archive_name, create_handoff, decode, extract_archive, handoff_fixture, raw, receipt_name,
        sha256_file_bounded,
    };

    fn claims() -> SourceClaims {
        SourceClaims {
            source_sha: "a".repeat(40),
            origin_run_id: 42,
            run_attempt: 2,
            source_ref: "refs/heads/main".to_owned(),
            event: "workflow_dispatch".to_owned(),
            version: "1.2.3".to_owned(),
        }
    }

    fn source() -> Source {
        Source::new(claims()).unwrap()
    }

    fn pending(source: &Source, target: &str, binary_sha256: String) -> PendingNotarization {
        PendingNotarization::new(PendingClaims {
            source: source.clone(),
            target: target.to_owned(),
            submission_id: "01234567-89ab-4cde-8fab-0123456789ab".to_owned(),
            binary_sha256,
            submission_zip_sha256: "b".repeat(64),
            signing_identity_sha1: "c".repeat(40),
            cdhash: "d".repeat(40),
        })
        .unwrap()
    }

    fn assert_invalid<T>(result: Result<T, QueueError>) {
        match result {
            Err(QueueError::Invalid(_) | QueueError::Json(_)) => {}
            Err(error @ (QueueError::Io { .. } | QueueError::Cleanup { .. })) => {
                panic!("unexpected IO or cleanup error: {error}")
            }
            Ok(_) => panic!("unsafe input accepted"),
        }
    }

    fn write(path: &Path, body: &[u8]) {
        crate::raw::write(path, body).unwrap();
    }

    #[test]
    fn native_tar_packaging_matches_the_strict_archive_contract() {
        let directory = crate::distribution::native_archive_fixture().unwrap();
        let mut origin = claims();
        origin.version = env!("CARGO_PKG_VERSION").to_owned();
        let source = Source::new(origin).unwrap();
        for target in TARGETS {
            let path = directory
                .path()
                .join("dist")
                .join(archive_name(&source, target));
            let binary = if target == "x86_64-pc-windows-msvc" {
                b"domyjob.exe".as_slice()
            } else {
                b"domyjob".as_slice()
            };
            assert_eq!(extract_archive(&path, &source, target).unwrap(), binary);
        }
    }

    fn archive(source: &Source, target: &str, extra: Option<(&str, tar::EntryType)>) -> Vec<u8> {
        let package = format!("domyjob-{}-{target}", source.version());
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut archive = tar::Builder::new(encoder);
        if let Some((name, kind)) = extra {
            append(&mut archive, name, kind, b"x");
        }
        for name in [&package, &format!("{package}/assets")] {
            append(&mut archive, name, tar::EntryType::Directory, b"");
        }
        let binary = if target == "x86_64-pc-windows-msvc" {
            "domyjob.exe"
        } else {
            "domyjob"
        };
        append(
            &mut archive,
            &format!("{package}/{binary}"),
            tar::EntryType::Regular,
            b"signed fixture",
        );
        for name in RESOURCES {
            append(
                &mut archive,
                &format!("{package}/{name}"),
                tar::EntryType::Regular,
                b"resource",
            );
        }
        archive.into_inner().unwrap().finish().unwrap()
    }

    fn append(
        builder: &mut tar::Builder<GzEncoder<Vec<u8>>>,
        name: &str,
        kind: tar::EntryType,
        body: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header
            .as_mut_bytes()
            .get_mut(..name.len())
            .unwrap()
            .copy_from_slice(name.as_bytes());
        header.set_entry_type(kind);
        header.set_size(u64::try_from(body.len()).unwrap());
        header.set_mode(0o600);
        header.set_cksum();
        builder.append(&header, body).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let directory = raw::temporary_directory().unwrap();
        let source = source();
        crate::raw::create_dir_all(&directory.path().join("notarization")).unwrap();
        let binary = directory.path().join("fixture-binary");
        write(&binary, b"signed fixture");
        let hash = sha256_file_bounded(&binary).unwrap();
        for target in TARGETS {
            let name = archive_name(&source, target);
            let path = directory.path().join(&name);
            write(&path, &archive(&source, target, None));
            let sha256 = sha256_file_bounded(&path).unwrap();
            write(
                &directory.path().join(format!("{name}.sha256")),
                format!("{sha256}  {name}\n").as_bytes(),
            );
        }
        for target in MAC_TARGETS {
            pending(&source, target, hash.clone())
                .store(
                    &directory
                        .path()
                        .join("notarization")
                        .join(receipt_name(target)),
                )
                .unwrap();
        }
        raw::remove_file(&binary).unwrap();
        directory
    }

    pub(super) fn build_handoff_fixture() -> (tempfile::TempDir, Handoff) {
        let directory = fixture();
        let original = source();
        create_handoff(directory.path(), &original).unwrap();
        let handoff = Handoff::inspect(directory.path(), &original).unwrap();
        (directory, handoff)
    }

    #[test]
    fn source_rejects_wrong_sha_ids_event_ref_and_noncanonical_version() {
        let cases: [fn(&mut SourceClaims); 10] = [
            |value| value.source_sha = "a".repeat(39),
            |value| value.source_sha = "A".repeat(40),
            |value| value.origin_run_id = 0,
            |value| value.run_attempt = 0,
            |value| value.source_ref = "refs/heads/feature".to_owned(),
            |value| value.event = "pull_request".to_owned(),
            |value| value.event = "push".to_owned(),
            |value| value.version = "01.2.3".to_owned(),
            |value| value.version = "1.2".to_owned(),
            |value| value.version = "../../outside".to_owned(),
        ];
        for change in cases {
            let mut value = claims();
            change(&mut value);
            assert_invalid(Source::new(value));
        }
        let mut tag = claims();
        tag.event = "push".to_owned();
        tag.source_ref = "refs/tags/v1.2.3".to_owned();
        assert_eq!(Source::new(tag).unwrap().publish_tag(), Some("v1.2.3"));
        let source = source();
        assert_eq!(source.source_sha(), "a".repeat(40));
        assert_eq!(source.origin_run_id(), 42);
        assert_eq!(source.run_attempt(), 2);
        assert_eq!(source.source_ref(), "refs/heads/main");
        assert_eq!(source.event(), "workflow_dispatch");
        assert_eq!(source.publish_tag(), None);
    }

    #[test]
    fn pending_schema_round_trips_and_rejects_tampering() {
        let receipt = pending(&source(), MAC_TARGETS.first().unwrap(), "a".repeat(64));
        let body = serde_json::to_vec(&receipt).unwrap();
        assert_eq!(decode::<PendingNotarization>(&body).unwrap(), receipt);
        assert_eq!(
            receipt.submission_id(),
            "01234567-89ab-4cde-8fab-0123456789ab"
        );
        assert_eq!(receipt.signing_identity_sha1(), "c".repeat(40));
        assert_eq!(receipt.cdhash(), "d".repeat(40));
        for (field, replacement) in [
            ("schemaVersion", json!(2)),
            ("target", json!("x86_64-unknown-linux-gnu")),
            (
                "submissionId",
                json!("00000000-0000-0000-0000-000000000000"),
            ),
            (
                "submissionId",
                json!("01234567-89AB-4cde-8fab-0123456789ab"),
            ),
            ("binarySha256", json!("a".repeat(63))),
            ("cdhash", json!("z".repeat(40))),
            ("unknown", json!(true)),
        ] {
            let mut value = serde_json::to_value(&receipt).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), replacement);
            assert_invalid(decode::<PendingNotarization>(
                &serde_json::to_vec(&value).unwrap(),
            ));
        }
    }

    #[test]
    fn states_and_deadlines_are_fail_closed() {
        for (label, expected) in [
            ("In Progress", NotaryState::InProgress),
            ("Accepted", NotaryState::Accepted),
            ("Invalid", NotaryState::Invalid),
        ] {
            let state = NotaryState::parse(label).unwrap();
            assert_eq!(state, expected);
            assert_eq!(state.with_deadline(100, 100 + DEADLINE - 1).unwrap(), state);
            assert_eq!(
                state.with_deadline(100, 100 + DEADLINE).unwrap(),
                NotaryState::Expired
            );
        }
        for label in ["", "accepted", "Rejected", "Expired", "In progress"] {
            assert_invalid(NotaryState::parse(label));
        }
        assert_invalid(NotaryState::Accepted.with_deadline(100, 99));
        assert_invalid(NotaryState::Accepted.with_deadline(u64::MAX, u64::MAX));
    }

    #[test]
    fn json_rejects_duplicate_fields_and_unknown_manifest_metadata() {
        let receipt = pending(&source(), MAC_TARGETS.first().unwrap(), "a".repeat(64));
        let body = serde_json::to_string(&receipt).unwrap();
        let duplicate = body.replacen('{', "{\"schemaVersion\":1,", 1);
        assert_invalid(decode::<PendingNotarization>(duplicate.as_bytes()));
        let (directory, _) = handoff_fixture();
        let path = directory.path().join("manifest.json");
        let manifest_body = super::read_bounded(&path, super::JSON_LIMIT).unwrap();
        let mut value: serde_json::Value = decode(&manifest_body).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_owned(), json!(true));
        assert_invalid(decode::<PendingManifest>(
            &serde_json::to_vec(&value).unwrap(),
        ));
        let manifest_duplicate = String::from_utf8(manifest_body).unwrap().replacen(
            '{',
            "{\"repository\":\"P4suta/domyjob\",",
            1,
        );
        assert_invalid(decode::<PendingManifest>(manifest_duplicate.as_bytes()));
    }

    #[test]
    fn changed_archive_after_inspection_fails_and_removes_owned_extraction() {
        let (directory, handoff) = handoff_fixture();
        let target = MAC_TARGETS.first().unwrap();
        write(handoff.archive(target).unwrap(), b"tampered archive");
        let extracted = raw::temporary_directory().unwrap();
        let owned = extracted.path().to_path_buf();
        assert_invalid(ExtractedBundle::extract_into(&handoff, extracted));
        assert!(
            matches!(raw::metadata(&owned), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
        assert_eq!(directory.path(), handoff.dir());
    }

    #[test]
    fn receipt_storage_is_atomic_and_refuses_existing_or_mismatched_source() {
        let directory = raw::temporary_directory().unwrap();
        let receipt = pending(&source(), MAC_TARGETS.first().unwrap(), "a".repeat(64));
        let path = directory.path().join("receipt.json");
        receipt.store(&path).unwrap();
        let initial = sha256_file_bounded(&path).unwrap();
        assert_invalid(receipt.store(&path));
        assert_eq!(sha256_file_bounded(&path).unwrap(), initial);
        assert_eq!(raw::read_dir(directory.path()).unwrap().count(), 1);
        let mut different = claims();
        different.origin_run_id = 99;
        assert_invalid(PendingNotarization::load(
            &path,
            &Source::new(different).unwrap(),
            receipt.target(),
        ));
        assert_invalid(PendingNotarization::load(
            &path,
            &source(),
            MAC_TARGETS.last().unwrap(),
        ));
    }

    #[test]
    fn handoff_binds_original_version_and_owned_extraction_cleans_up() {
        let (directory, handoff) = handoff_fixture();
        let original = source();
        let manifest = PendingManifest::load(&directory.path().join("manifest.json")).unwrap();
        assert_eq!(manifest.source().version(), "1.2.3");
        assert_eq!(handoff.source(), &original);
        assert_eq!(handoff.dir(), directory.path());
        assert_eq!(handoff.archives().count(), 5);
        let extracted = ExtractedBundle::extract(&handoff).unwrap();
        let binary = extracted
            .binary(MAC_TARGETS.first().unwrap())
            .unwrap()
            .to_path_buf();
        assert_eq!(
            sha256_file_bounded(&binary).unwrap(),
            extracted
                .receipt(MAC_TARGETS.first().unwrap())
                .unwrap()
                .binary_sha256()
        );
        extracted.close().unwrap();
        assert!(
            matches!(raw::metadata(&binary), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
        assert_invalid(create_handoff(directory.path(), &original));
        let mut different = claims();
        different.version = "1.2.4".to_owned();
        assert_invalid(Handoff::inspect(
            directory.path(),
            &Source::new(different).unwrap(),
        ));
    }

    #[test]
    fn handoff_rejects_checksum_and_manifest_source_tampering() {
        let directory = fixture();
        let original = source();
        create_handoff(directory.path(), &original).unwrap();
        let name = archive_name(&original, TARGETS.first().unwrap());
        write(
            &directory.path().join(format!("{name}.sha256")),
            b"bad checksum\n",
        );
        assert_invalid(Handoff::inspect(directory.path(), &original));
        let manifest_path = directory.path().join("manifest.json");
        let mut manifest: serde_json::Value =
            decode(&super::read_bounded(&manifest_path, super::JSON_LIMIT).unwrap()).unwrap();
        manifest
            .get_mut("source")
            .unwrap()
            .get_mut("sourceSha")
            .unwrap()
            .clone_from(&json!("b".repeat(40)));
        write(&manifest_path, &serde_json::to_vec(&manifest).unwrap());
        assert_invalid(PendingManifest::load(&manifest_path));
    }

    #[test]
    fn archive_rejects_links_traversal_duplicates_aliases_and_foreign_versions() {
        let directory = raw::temporary_directory().unwrap();
        let original = source();
        let target = MAC_TARGETS.first().unwrap();
        let path = directory.path().join("archive.tar.gz");
        let duplicate = format!("domyjob-1.2.3-{target}/domyjob");
        let alias = format!("domyjob-1.2.3-{target}/DOMYJOB");
        for (name, kind) in [
            ("../outside", tar::EntryType::Regular),
            ("/outside", tar::EntryType::Regular),
            ("link", tar::EntryType::Symlink),
            ("hard", tar::EntryType::Link),
            (duplicate.as_str(), tar::EntryType::Regular),
            (alias.as_str(), tar::EntryType::Regular),
        ] {
            write(&path, &archive(&original, target, Some((name, kind))));
            assert_invalid(extract_archive(&path, &original, target));
        }
        write(&path, &archive(&original, target, None));
        let mut foreign = claims();
        foreign.version = "1.2.4".to_owned();
        assert_invalid(extract_archive(
            &path,
            &Source::new(foreign).unwrap(),
            target,
        ));
    }
}
