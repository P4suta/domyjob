use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::release_orchestration::OrchestrationError;
use crate::release_queue::{Handoff, MAC_TARGETS, Source, SourceClaims, sha256_file_bounded};

pub(crate) const RECEIPT: &str = "release-derivation.json";
pub(crate) const BUILD_MANIFEST: &str = "build-manifest.json";
const LIMIT: u64 = 64 * 1024 * 1024;

fn invalid(reason: &str) -> OrchestrationError {
    OrchestrationError::Invalid(reason.to_owned())
}

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "accepted release staging writes only owned public files and bounded artifact reads"
    )]

    pub(super) fn rename(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>, OrchestrationError> {
    crate::release_queue::read_bounded(path, limit).map_err(Into::into)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Asset {
    name: String,
    original_sha256: String,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Derivation {
    schema_version: u8,
    operation: String,
    source: Source,
    producer: Finalizer,
    pending_manifest_sha256: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "SourceClaims", into = "SourceClaims")]
pub(crate) struct Finalizer {
    source: Source,
    event: String,
}

impl TryFrom<SourceClaims> for Finalizer {
    type Error = OrchestrationError;

    fn try_from(mut claims: SourceClaims) -> Result<Self, Self::Error> {
        if !matches!(
            claims.event.as_str(),
            "schedule" | "workflow_run" | "workflow_dispatch"
        ) || claims.source_ref != "refs/heads/main"
        {
            return Err(invalid(
                "derivation requires a trusted main finalizer event",
            ));
        }
        let event = std::mem::replace(&mut claims.event, "workflow_dispatch".to_owned());
        Ok(Self {
            source: Source::new(claims)?,
            event,
        })
    }
}

impl From<Finalizer> for SourceClaims {
    fn from(producer: Finalizer) -> Self {
        let mut claims = Self::from(producer.source);
        claims.event = producer.event;
        claims
    }
}

impl Finalizer {
    pub(crate) const fn source(&self) -> &Source {
        &self.source
    }
    pub(crate) fn event(&self) -> &str {
        &self.event
    }
}

#[derive(Debug)]
pub(crate) struct ReadyDistribution {
    directory: PathBuf,
    derivation: Derivation,
    receipt_sha256: String,
}

impl ReadyDistribution {
    pub(crate) fn create(
        handoff: &Handoff,
        accepted: &crate::release::AcceptedToken,
        producer: Finalizer,
        destination: &Path,
    ) -> Result<Self, OrchestrationError> {
        if accepted.source() != handoff.source() {
            return Err(invalid("package acceptance belongs to another source"));
        }
        match std::fs::symlink_metadata(destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => return Err(invalid("ready output already exists")),
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("ready output has no parent"))?;
        let staging = tempfile::Builder::new()
            .prefix(".ready-")
            .tempdir_in(parent)?;
        let derivation = prepare(handoff, accepted, producer, staging.path())?;
        crate::raw::write(
            &staging.path().join(RECEIPT),
            &serde_json::to_vec_pretty(&derivation)
                .map_err(|error| invalid(&format!("serializing release derivation: {error}")))?,
        )?;
        Self::inspect(staging.path(), handoff)?;
        promote(staging, destination)?;
        Self::inspect(destination, handoff)
    }

    pub(crate) fn inspect(directory: &Path, handoff: &Handoff) -> Result<Self, OrchestrationError> {
        let receipt = directory.join(RECEIPT);
        let derivation: Derivation =
            domyjob_core::ingress::json(&read(&receipt, 1_048_576)?, 1_048_576)?;
        validate(&derivation, handoff)?;
        let expected: BTreeSet<_> = [RECEIPT.to_owned(), BUILD_MANIFEST.to_owned()]
            .into_iter()
            .chain(
                derivation
                    .assets
                    .iter()
                    .flat_map(|asset| [asset.name.clone(), format!("{}.sha256", asset.name)]),
            )
            .collect();
        inventory(directory, &expected)?;
        if sha256_file_bounded(&directory.join(BUILD_MANIFEST))?
            != derivation.pending_manifest_sha256
        {
            return Err(invalid(
                "public build manifest differs from the original build",
            ));
        }
        for asset in &derivation.assets {
            if sha256_file_bounded(&directory.join(&asset.name))? != asset.sha256
                || read(&directory.join(format!("{}.sha256", asset.name)), 512)?
                    != format!("{}  {}\n", asset.sha256, asset.name).as_bytes()
            {
                return Err(invalid("ready asset or checksum changed after stapling"));
            }
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            derivation,
            receipt_sha256: sha256_file_bounded(&receipt)?,
        })
    }

    pub(crate) fn verify_native(&self, handoff: &Handoff) -> Result<(), OrchestrationError> {
        for target in MAC_TARGETS {
            let name = package_name(handoff.source(), target);
            crate::macos_package::verify_stapled(
                &self.directory.join(name),
                handoff.receipt(target)?,
            )
            .map_err(|error| invalid(&format!("stapled distribution: {error}")))?;
        }
        self.unchanged(handoff)
    }

    pub(crate) fn unchanged(&self, handoff: &Handoff) -> Result<(), OrchestrationError> {
        if sha256_file_bounded(&self.directory.join(RECEIPT))? != self.receipt_sha256 {
            return Err(invalid("verified release derivation changed"));
        }
        Self::inspect(&self.directory, handoff)?;
        Ok(())
    }

    pub(crate) const fn source(&self) -> &Source {
        &self.derivation.source
    }

    pub(crate) const fn producer(&self) -> &Finalizer {
        &self.derivation.producer
    }

    pub(crate) fn subjects(&self) -> Vec<(PathBuf, String)> {
        std::iter::once((self.directory.join(RECEIPT), self.receipt_sha256.clone()))
            .chain(
                self.derivation
                    .assets
                    .iter()
                    .filter(|asset| is_package(&asset.name))
                    .map(|asset| (self.directory.join(&asset.name), asset.sha256.clone())),
            )
            .collect()
    }

    pub(crate) fn build_manifest(&self) -> (PathBuf, String) {
        (
            self.directory.join(BUILD_MANIFEST),
            self.derivation.pending_manifest_sha256.clone(),
        )
    }

    pub(crate) fn files(&self) -> Result<Vec<(String, PathBuf, String)>, OrchestrationError> {
        let (manifest, manifest_sha256) = self.build_manifest();
        let mut files = vec![
            (
                RECEIPT.to_owned(),
                self.directory.join(RECEIPT),
                self.receipt_sha256.clone(),
            ),
            (BUILD_MANIFEST.to_owned(), manifest, manifest_sha256),
        ];
        for asset in &self.derivation.assets {
            if MAC_TARGETS.iter().any(|target| {
                asset.name == format!("domyjob-{}-{target}.tar.gz", self.source().version())
            }) {
                continue;
            }
            files.push((
                asset.name.clone(),
                self.directory.join(&asset.name),
                asset.sha256.clone(),
            ));
            let name = format!("{}.sha256", asset.name);
            let path = self.directory.join(&name);
            files.push((name, path.clone(), sha256_file_bounded(&path)?));
        }
        Ok(files)
    }
}

fn promote(staging: tempfile::TempDir, destination: &Path) -> Result<(), OrchestrationError> {
    raw::rename(staging.path(), destination)?;
    match staging.close() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn package_name(source: &Source, target: &str) -> String {
    format!("domyjob-{}-{target}.pkg", source.version())
}

fn is_package(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "pkg")
}

fn prepare(
    handoff: &Handoff,
    accepted: &crate::release::AcceptedToken,
    producer: Finalizer,
    directory: &Path,
) -> Result<Derivation, OrchestrationError> {
    let manifest = handoff.dir().join("manifest.json");
    let pending_manifest_sha256 = sha256_file_bounded(&manifest)?;
    crate::raw::write(&directory.join(BUILD_MANIFEST), &read(&manifest, LIMIT)?)?;
    if sha256_file_bounded(&directory.join(BUILD_MANIFEST))? != pending_manifest_sha256 {
        return Err(invalid(
            "build manifest changed while preparing accepted distribution",
        ));
    }
    let mut assets = Vec::new();
    for (name, path, original_sha256) in handoff.archives() {
        let destination = directory.join(name);
        if let Some(target) = MAC_TARGETS
            .into_iter()
            .find(|target| name == package_name(handoff.source(), target))
        {
            let package = crate::macos_package::verify_pending(
                handoff.package(target)?,
                handoff.receipt(target)?,
            )
            .map_err(|error| invalid(&format!("pending package: {error}")))?;
            let stapled = crate::macos_package::staple(package, accepted, directory)
                .map_err(|error| invalid(&format!("stapling package: {error}")))?;
            if stapled.path() != destination
                || sha256_file_bounded(&destination)? != stapled.sha256()
            {
                return Err(invalid(
                    "stapling returned another package or changed digest",
                ));
            }
        } else {
            crate::raw::write(&destination, &read(path, LIMIT)?)?;
            if sha256_file_bounded(&destination)? != original_sha256 {
                return Err(invalid(
                    "archive changed while preparing accepted distribution",
                ));
            }
        }
        let sha256 = sha256_file_bounded(&destination)?;
        crate::raw::write(
            &directory.join(format!("{name}.sha256")),
            format!("{sha256}  {name}\n").as_bytes(),
        )?;
        assets.push(Asset {
            name: name.to_owned(),
            original_sha256: original_sha256.to_owned(),
            sha256,
        });
    }
    Ok(Derivation {
        schema_version: 1,
        operation: "apple-notarization-staple".to_owned(),
        source: handoff.source().clone(),
        producer,
        pending_manifest_sha256,
        assets,
    })
}

fn validate(derivation: &Derivation, handoff: &Handoff) -> Result<(), OrchestrationError> {
    if derivation.schema_version != 1
        || derivation.operation != "apple-notarization-staple"
        || &derivation.source != handoff.source()
        || derivation.producer.source().version() != handoff.source().version()
        || derivation.pending_manifest_sha256
            != sha256_file_bounded(&handoff.dir().join("manifest.json"))?
    {
        return Err(invalid(
            "release derivation source, operation, or producer mismatch",
        ));
    }
    let original: Vec<_> = handoff.archives().collect();
    if original.len() != 7 || derivation.assets.len() != original.len() {
        return Err(invalid(
            "ready distribution requires five archives and two packages",
        ));
    }
    let mut names = BTreeSet::new();
    for asset in &derivation.assets {
        if !names.insert(&asset.name)
            || asset.sha256.len() != 64
            || !asset
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !original.iter().any(|(name, _path, hash)| {
                *name == asset.name
                    && *hash == asset.original_sha256
                    && (is_package(name) || asset.sha256 == asset.original_sha256)
            })
        {
            return Err(invalid(
                "release derivation substituted an asset or original digest",
            ));
        }
    }
    Ok(())
}

fn inventory(directory: &Path, expected: &BTreeSet<String>) -> Result<(), OrchestrationError> {
    if !std::fs::symlink_metadata(directory)?.file_type().is_dir() {
        return Err(invalid("ready directory cannot be an alias or link"));
    }
    let mut observed = BTreeSet::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(invalid("ready inventory cannot contain aliases or links"));
        }
        observed.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_name| invalid("ready file name is not UTF-8"))?,
        );
    }
    if &observed != expected {
        return Err(invalid(
            "ready distribution inventory differs from its derivation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Asset, BUILD_MANIFEST, Derivation, Finalizer, Handoff, LIMIT, RECEIPT, ReadyDistribution,
        SourceClaims, is_package, promote, read, sha256_file_bounded,
    };
    use crate::release_queue::MAC_TARGETS;
    use std::path::Path;

    fn fixture(handoff: &Handoff) -> (tempfile::TempDir, Derivation) {
        let directory = tempfile::tempdir().unwrap();
        crate::raw::write(
            &directory.path().join(BUILD_MANIFEST),
            &read(&handoff.dir().join("manifest.json"), LIMIT).unwrap(),
        )
        .unwrap();
        let mut assets = Vec::new();
        for (name, path, original_sha256) in handoff.archives() {
            let bytes = if is_package(name) {
                b"package with a stapled ticket".to_vec()
            } else {
                read(path, LIMIT).unwrap()
            };
            let destination = directory.path().join(name);
            crate::raw::write(&destination, &bytes).unwrap();
            let sha256 = sha256_file_bounded(&destination).unwrap();
            crate::raw::write(
                &directory.path().join(format!("{name}.sha256")),
                format!("{sha256}  {name}\n").as_bytes(),
            )
            .unwrap();
            assets.push(Asset {
                name: name.to_owned(),
                original_sha256: original_sha256.to_owned(),
                sha256,
            });
        }
        let producer = Finalizer::try_from(SourceClaims {
            source_sha: "c".repeat(40),
            origin_run_id: 99,
            run_attempt: 1,
            source_ref: "refs/heads/main".to_owned(),
            event: "workflow_dispatch".to_owned(),
            version: handoff.source().version().to_owned(),
        })
        .unwrap();
        let derivation = Derivation {
            schema_version: 1,
            operation: "apple-notarization-staple".to_owned(),
            source: handoff.source().clone(),
            producer,
            pending_manifest_sha256: sha256_file_bounded(&handoff.dir().join("manifest.json"))
                .unwrap(),
            assets,
        };
        store(directory.path(), &derivation);
        (directory, derivation)
    }

    fn store(directory: &Path, derivation: &Derivation) {
        crate::raw::write(
            &directory.join(RECEIPT),
            &serde_json::to_vec_pretty(derivation).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn ready_digest_records_preserve_original_build_and_changed_package_lineage() {
        let (_pending, handoff) = crate::release_queue::package_handoff_fixture();
        let (directory, derivation) = fixture(&handoff);
        let ready = ReadyDistribution::inspect(directory.path(), &handoff).unwrap();
        assert_eq!(ready.subjects().len(), 3);
        assert_eq!(ready.files().unwrap().len(), 12);
        assert_eq!(
            ready.build_manifest().1,
            sha256_file_bounded(&handoff.dir().join("manifest.json")).unwrap()
        );
        assert!(ready.files().unwrap().iter().all(|(name, _path, _hash)| {
            !MAC_TARGETS
                .iter()
                .any(|target| name.contains(target) && name.contains(".tar.gz"))
        }));
        for asset in &derivation.assets {
            assert_eq!(
                asset.sha256 == asset.original_sha256,
                !is_package(&asset.name)
            );
        }
        ready.unchanged(&handoff).unwrap();
        crate::raw::write(&directory.path().join(RECEIPT), b"{}").unwrap();
        ready.unchanged(&handoff).unwrap_err();
    }

    #[test]
    fn ready_rejects_changed_tar_or_foreign_original_source_and_digest() {
        let (_pending, handoff) = crate::release_queue::package_handoff_fixture();
        let (directory, mut derivation) = fixture(&handoff);
        let original = serde_json::to_vec(&derivation).unwrap();
        derivation.assets.first_mut().unwrap().sha256 = "f".repeat(64);
        store(directory.path(), &derivation);
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
        derivation = domyjob_core::ingress::json(&original, 1_048_576).unwrap();
        derivation.assets.get_mut(5).unwrap().original_sha256 = "f".repeat(64);
        store(directory.path(), &derivation);
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
        derivation = domyjob_core::ingress::json(&original, 1_048_576).unwrap();
        derivation.pending_manifest_sha256 = "f".repeat(64);
        store(directory.path(), &derivation);
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
    }

    #[test]
    fn ready_rejects_partial_duplicate_and_unlisted_files() {
        let (_pending, handoff) = crate::release_queue::package_handoff_fixture();
        let (directory, mut derivation) = fixture(&handoff);
        let missing = derivation.assets.pop().unwrap();
        store(directory.path(), &derivation);
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
        derivation
            .assets
            .push(derivation.assets.first().unwrap().clone());
        store(directory.path(), &derivation);
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
        derivation.assets.pop();
        derivation.assets.push(missing);
        store(directory.path(), &derivation);
        crate::raw::write(&directory.path().join("unreviewed.pkg"), b"unreviewed").unwrap();
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
    }

    #[test]
    fn ready_rejects_substitution_or_removal_of_the_public_build_manifest() {
        let (_pending, handoff) = crate::release_queue::package_handoff_fixture();
        let (directory, _derivation) = fixture(&handoff);
        let manifest = directory.path().join(BUILD_MANIFEST);
        crate::raw::write(&manifest, b"{}").unwrap();
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
        let relocated = tempfile::tempdir().unwrap();
        super::raw::rename(&manifest, &relocated.path().join(BUILD_MANIFEST)).unwrap();
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
    }

    #[test]
    fn legacy_pending_cannot_be_mislabeled_as_stapled_distribution() {
        let (_pending, handoff) = crate::release_queue::handoff_fixture();
        let (directory, _derivation) = fixture(&handoff);
        ReadyDistribution::inspect(directory.path(), &handoff).unwrap_err();
    }

    #[test]
    fn ready_promotion_preserves_output_without_a_false_cleanup_failure() {
        let root = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir_in(root.path()).unwrap();
        let original = staging.path().to_path_buf();
        crate::raw::write(&original.join("public.pkg"), b"accepted bytes").unwrap();
        let destination = root.path().join("ready");
        promote(staging, &destination).unwrap();
        assert_eq!(
            read(&destination.join("public.pkg"), 64).unwrap(),
            b"accepted bytes"
        );
        assert_eq!(
            std::fs::symlink_metadata(original).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn derivation_preserves_the_actual_finalizer_event() {
        for event in ["schedule", "workflow_run", "workflow_dispatch"] {
            let producer = Finalizer::try_from(SourceClaims {
                source_sha: "a".repeat(40),
                origin_run_id: 99,
                run_attempt: 1,
                source_ref: "refs/heads/main".to_owned(),
                event: event.to_owned(),
                version: "1.2.3".to_owned(),
            })
            .unwrap();
            let bytes = serde_json::to_vec(&producer).unwrap();
            let replayed: Finalizer = domyjob_core::ingress::json(&bytes, 1_048_576).unwrap();
            assert_eq!(replayed.event(), event);
            assert_eq!(SourceClaims::from(replayed).event, event);
        }
    }
}
