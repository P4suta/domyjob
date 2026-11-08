use std::io;
use std::path::{Path, PathBuf};

use domyjob_core::cache_key::{
    CacheAddress, CacheBinding, CacheKey, DIGEST_BYTES, project_binding, slot_binding,
};
use domyjob_core::domain::JobId;
use domyjob_core::scope_ownership::{Binding, NamespaceMatch, authorize};
use domyjob_core::storage_lifecycle::{
    CacheLease, CacheUses, Quiescence, cache_may_retire, cache_use_may_release,
};
use serde::{Deserialize, Serialize};

use crate::lock::OsLock;
use crate::process::resources::ScopeNamespace;
use crate::state_io;

const LAYOUTS: [Layout; 3] = [Layout::Compact, Layout::Expanded, Layout::Legacy];

const INPUT_FILES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "mise.toml",
    ".mise.toml",
    "rust-toolchain.toml",
    "rust-toolchain",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "uv.lock",
    "poetry.lock",
    "go.sum",
    "mix.lock",
];
const CACHE_TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n# domyjob build cache\n";

pub(crate) fn key(root: &Path, archive: &[u8]) -> io::Result<CacheKey> {
    let canonical = root.canonicalize()?;
    let project = *blake3::hash(canonical.as_os_str().as_encoded_bytes()).as_bytes();
    let mut inputs = blake3::Hasher::new();
    for entry in tar::Archive::new(archive).entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let selected = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| INPUT_FILES.contains(&name))
            || path.to_str() == Some(".cargo/config.toml");
        if !selected {
            continue;
        }
        let text = path
            .to_str()
            .ok_or_else(|| io::Error::other("cache input path is not UTF-8"))?;
        inputs.update(text.as_bytes());
        inputs.update(&[0]);
        let limit =
            usize::try_from(domyjob_core::wire::MAX_SNAPSHOT_BYTES).map_err(io::Error::other)?;
        let bytes = crate::bounded::read(&mut entry, limit)?
            .ok_or_else(|| io::Error::other("cache inputs exceed the source limit"))?;
        inputs.update(
            &u64::try_from(bytes.len())
                .map_err(io::Error::other)?
                .to_be_bytes(),
        );
        inputs.update(&bytes);
    }
    Ok(CacheKey::new(project, *inputs.finalize().as_bytes()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Compact,
    Expanded,
    Legacy,
}

impl Layout {
    fn digest(self, bytes: [u8; DIGEST_BYTES]) -> String {
        match self {
            Self::Compact => domyjob_core::cache_key::route(bytes)
                .into_iter()
                .map(char::from)
                .collect(),
            Self::Expanded => domyjob_core::cache_key::address(bytes)
                .into_iter()
                .map(char::from)
                .collect(),
            Self::Legacy => blake3::Hash::from_bytes(bytes).to_hex().to_string(),
        }
    }

    const fn keys(self) -> &'static str {
        match self {
            Self::Compact | Self::Expanded => "k",
            Self::Legacy => "keys",
        }
    }

    const fn target(self) -> &'static str {
        match self {
            Self::Compact | Self::Expanded => "t",
            Self::Legacy => "target",
        }
    }
}

fn io_error(error: impl std::error::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Usage {
    namespace: ScopeNamespace,
    job: JobId,
    key: CacheKey,
}

impl Usage {
    pub(crate) fn for_job((namespace, job): (ScopeNamespace, &JobId), key: CacheKey) -> Self {
        Self {
            namespace,
            job: job.clone(),
            key,
        }
    }

    fn path(&self, root: &Path, layout: Layout) -> io::Result<PathBuf> {
        let encoded = serde_json::to_vec(self).map_err(io_error)?;
        let identity = blake3::hash(&encoded).to_hex().to_string();
        Ok(root
            .join(layout.digest(self.key.project()))
            .join(layout.keys())
            .join(layout.digest(self.key.inputs()))
            .join("uses")
            .join(identity))
    }

    fn matches(&self, actual: &Self) -> bool {
        authorize(Binding {
            namespace: if self.namespace == actual.namespace {
                NamespaceMatch::Same
            } else {
                NamespaceMatch::Foreign
            },
            job_matches: self.job == actual.job,
            identity_matches: self.key == actual.key,
            slot_matches: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn retained(&self, root: &Path) -> io::Result<bool> {
        for layout in LAYOUTS {
            if state_io::read_bytes(&self.path(root, layout)?)
                .map_err(io_error)?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub(crate) fn release_use(root: &Path, usage: &Usage, quiescence: Quiescence) -> io::Result<()> {
    if !cache_use_may_release(quiescence) {
        return Ok(());
    }
    let selections = selections(root, usage.key.project())?;
    let mut claims = Vec::new();
    for layout in LAYOUTS {
        let path = usage.path(root, layout)?;
        if let Some(bytes) = state_io::read_bytes(&path).map_err(io_error)? {
            let saved: Usage =
                domyjob_core::ingress::json(&bytes, domyjob_core::wire::MAX_CONTROL_BYTES)
                    .map_err(io_error)?;
            if !saved.matches(usage) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cache use is not bound to this execution",
                ));
            }
            claims.push(path);
        }
    }
    for claim in claims {
        state_io::remove_file(&claim).map_err(io_error)?;
    }
    prune_selected(root, usage.key.project(), &selections)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn record<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    state_io::read_bytes(path)
        .map_err(io_error)?
        .map(|bytes| {
            domyjob_core::ingress::json(&bytes, domyjob_core::wire::MAX_CONTROL_BYTES)
                .map_err(io_error)
        })
        .transpose()
}

fn write_record(path: &Path, value: &impl Serialize) -> io::Result<()> {
    state_io::write_bytes(path, &serde_json::to_vec(value).map_err(io_error)?).map_err(io_error)
}

fn bind_project(project: &Path, key: CacheKey) -> io::Result<()> {
    let path = project.join("project.key");
    match project_binding(key.project(), record(&path)?) {
        CacheBinding::Matching => Ok(()),
        CacheBinding::Conflicting => Err(invalid(
            "cache routing prefix belongs to another full project identity",
        )),
        CacheBinding::Vacant => {
            for entry in std::fs::read_dir(project)? {
                if entry?.file_name() != "selection.lock" {
                    return Err(invalid(
                        "an unbound cache project contains existing artifacts",
                    ));
                }
            }
            write_record(&path, &key.project())
        }
    }
}

fn bind_slot(slot: &Path, key: CacheKey) -> io::Result<()> {
    let path = slot.join("slot.key");
    match slot_binding(key, record(&path)?) {
        CacheBinding::Matching => Ok(()),
        CacheBinding::Conflicting => Err(invalid(
            "cache routing prefix belongs to another full input identity",
        )),
        CacheBinding::Vacant => {
            if std::fs::read_dir(slot)?.next().transpose()?.is_some() {
                return Err(invalid("an unbound cache slot contains existing artifacts"));
            }
            write_record(&path, &key)
        }
    }
}

struct Selection {
    _guard: OsLock,
    key: Option<CacheKey>,
}

struct OldSelection {
    _guard: OsLock,
    project: PathBuf,
    layout: Layout,
    latest: Option<Vec<u8>>,
}

struct Selections {
    compact: Option<Selection>,
    old: Vec<OldSelection>,
}

fn existing_old_slot(selection: &OldSelection, latest: &[u8]) -> io::Result<()> {
    let name = std::str::from_utf8(latest).map_err(io_error)?;
    let slot = selection.project.join(selection.layout.keys()).join(name);
    state_io::existing_directory(&slot)
        .map_err(io_error)?
        .ok_or_else(|| invalid("the selected old cache slot is missing"))?;
    Ok(())
}

fn admit_old_selection(selection: &OldSelection, old: &[OldSelection]) -> io::Result<()> {
    let Some(latest) = &selection.latest else {
        return Ok(());
    };
    if CacheAddress::Route.admits(latest) {
        return Ok(());
    }
    let own = match selection.layout {
        Layout::Expanded => CacheAddress::Expanded,
        Layout::Legacy => CacheAddress::Legacy,
        Layout::Compact => return Err(invalid("compact selection requires a full key record")),
    };
    if own.admits(latest) {
        return existing_old_slot(selection, latest);
    }
    if selection.layout == Layout::Legacy && CacheAddress::Expanded.admits(latest) {
        let expanded = old
            .iter()
            .find(|candidate| candidate.layout == Layout::Expanded)
            .filter(|candidate| candidate.latest.as_ref() == Some(latest))
            .ok_or_else(|| invalid("legacy expanded marker has no matching expanded selection"))?;
        return existing_old_slot(expanded, latest);
    }
    Err(invalid(
        "old cache selection has an invalid routing address",
    ))
}

fn selections(root: &Path, digest: [u8; DIGEST_BYTES]) -> io::Result<Selections> {
    let compact = selected(root, digest)?;
    let mut old = Vec::new();
    for layout in [Layout::Expanded, Layout::Legacy] {
        let project = root.join(layout.digest(digest));
        let Some(guard) = OsLock::exclusive_in_existing_directory(&project.join("selection.lock"))
            .map_err(io_error)?
        else {
            continue;
        };
        let latest = state_io::read_bytes(&project.join("current")).map_err(io_error)?;
        old.push(OldSelection {
            _guard: guard,
            project,
            layout,
            latest,
        });
    }
    for selection in &old {
        admit_old_selection(selection, &old)?;
    }
    Ok(Selections { compact, old })
}

fn existing_slot(project: &Path, key: CacheKey) -> io::Result<()> {
    let slot = project
        .join(Layout::Compact.keys())
        .join(Layout::Compact.digest(key.inputs()));
    let _retained = state_io::existing_directory(&slot)
        .map_err(io_error)?
        .ok_or_else(|| invalid("the selected cache slot is missing"))?;
    if slot_binding(key, record(&slot.join("slot.key"))?) != CacheBinding::Matching {
        return Err(invalid(
            "the selected cache slot is not bound to its full identity",
        ));
    }
    Ok(())
}

fn selected(root: &Path, project_digest: [u8; DIGEST_BYTES]) -> io::Result<Option<Selection>> {
    let project = root.join(Layout::Compact.digest(project_digest));
    let Some(guard) = OsLock::exclusive_in_existing_directory(&project.join("selection.lock"))
        .map_err(io_error)?
    else {
        return Ok(None);
    };
    let key: Option<CacheKey> = record(&project.join("current"))?;
    let saved: Option<[u8; DIGEST_BYTES]> = record(&project.join("project.key"))?;
    match project_binding(project_digest, saved) {
        CacheBinding::Conflicting => {
            return Err(invalid(
                "cache selection belongs to another full project identity",
            ));
        }
        CacheBinding::Vacant if key.is_some() => {
            return Err(invalid("cache selection has no full project binding"));
        }
        CacheBinding::Matching | CacheBinding::Vacant => {}
    }
    if let Some(key) = key {
        if project_binding(project_digest, Some(key.project())) != CacheBinding::Matching {
            return Err(invalid(
                "cache selection records another full project identity",
            ));
        }
        existing_slot(&project, key)?;
    }
    Ok(Some(Selection { _guard: guard, key }))
}

fn pending_uses(slot: &Path) -> io::Result<CacheUses> {
    let directory = slot.join("uses");
    match std::fs::symlink_metadata(&directory) {
        Ok(_) => state_io::private_dir(&directory).map_err(io_error)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(CacheUses::Pending),
        Err(error) => return Err(error),
    }
    Ok(
        if std::fs::read_dir(directory)?.next().transpose()?.is_some() {
            CacheUses::Pending
        } else {
            CacheUses::Released
        },
    )
}

fn sweep(project: &Path, layout: Layout, latest: &[u8]) -> io::Result<()> {
    let keys = project.join(layout.keys());
    let trash = project.join("trash");
    for entry in std::fs::read_dir(&keys)? {
        let entry = entry?;
        let name = entry.file_name();
        let superseded = name.as_encoded_bytes() != latest;
        if !entry.file_type()?.is_dir() {
            return Err(io::Error::other("cache key is not a directory"));
        }
        if layout == Layout::Compact {
            let saved: CacheKey = record(&entry.path().join("slot.key"))?
                .ok_or_else(|| invalid("cache slot has no full identity"))?;
            let expected: [u8; DIGEST_BYTES] = record(&project.join("project.key"))?
                .ok_or_else(|| invalid("cache project has no full identity"))?;
            if project_binding(expected, Some(saved.project())) != CacheBinding::Matching
                || Layout::Compact.digest(saved.inputs()).as_bytes() != name.as_encoded_bytes()
            {
                return Err(invalid(
                    "cache slot identity does not match its routed directory",
                ));
            }
        }
        let guard = OsLock::try_exclusive(&entry.path().join("use.lock")).map_err(io_error)?;
        let lease = if guard.is_some() {
            CacheLease::Released
        } else {
            CacheLease::Held
        };
        if cache_may_retire(superseded, lease, pending_uses(&entry.path())?) {
            drop(guard);
            state_io::set_aside(&entry.path(), &trash).map_err(io_error)?;
        }
    }
    for leftover in state_io::empty(&trash).map_err(io_error)? {
        eprintln!(
            "domyjob: keeping retired cache {}: {}",
            leftover.path.display(),
            leftover.error
        );
    }
    Ok(())
}

fn prune_selected(
    root: &Path,
    digest: [u8; DIGEST_BYTES],
    selections: &Selections,
) -> io::Result<()> {
    let key = selections
        .compact
        .as_ref()
        .and_then(|selection| selection.key);
    if let Some(key) = key {
        for old in selections.old.iter().rev() {
            if old.latest.is_some() {
                state_io::write_bytes(
                    &old.project.join("current"),
                    Layout::Compact.digest(key.inputs()).as_bytes(),
                )
                .map_err(io_error)?;
            }
        }
    }
    if let Some(key) = key {
        sweep(
            &root.join(Layout::Compact.digest(digest)),
            Layout::Compact,
            Layout::Compact.digest(key.inputs()).as_bytes(),
        )?;
    }
    for old in &selections.old {
        let Some(latest) = &old.latest else {
            continue;
        };
        if key.is_none()
            && (CacheAddress::Route.admits(latest)
                || (old.layout == Layout::Legacy && CacheAddress::Expanded.admits(latest)))
        {
            continue;
        }
        let latest = key.map_or_else(
            || latest.clone(),
            |key| Layout::Compact.digest(key.inputs()).into_bytes(),
        );
        sweep(&old.project, old.layout, &latest)?;
    }
    Ok(())
}

fn prune(root: &Path, digest: [u8; DIGEST_BYTES]) -> io::Result<()> {
    let selections = selections(root, digest)?;
    prune_selected(root, digest, &selections)
}

#[derive(Debug)]
pub(crate) struct Lease {
    root: PathBuf,
    key: CacheKey,
    target: PathBuf,
    guard: Option<OsLock>,
}

impl Lease {
    pub(crate) fn acquire(root: &Path, key: CacheKey, usage: &Usage) -> io::Result<Self> {
        Self::acquire_layout(root, key, usage, Layout::Compact)
    }

    fn acquire_layout(
        root: &Path,
        key: CacheKey,
        usage: &Usage,
        layout: Layout,
    ) -> io::Result<Self> {
        if usage.key != key {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache claim has another key",
            ));
        }
        state_io::private_dir(root).map_err(io_error)?;
        state_io::write_bytes(&root.join("CACHEDIR.TAG"), CACHE_TAG).map_err(io_error)?;
        let project = root.join(layout.digest(key.project()));
        state_io::private_dir(&project).map_err(io_error)?;
        let selection = OsLock::exclusive(&project.join("selection.lock")).map_err(io_error)?;
        if layout == Layout::Compact {
            bind_project(&project, key)?;
        }
        let name = layout.digest(key.inputs());
        let slot = project.join(layout.keys()).join(&name);
        state_io::private_dir(&slot).map_err(io_error)?;
        if layout == Layout::Compact {
            bind_slot(&slot, key)?;
        }
        let guard =
            OsLock::shared_file(state_io::open_lock(&slot.join("use.lock")).map_err(io_error)?)?;
        let target = slot.join(layout.target());
        state_io::private_dir(&target).map_err(io_error)?;
        let path = usage.path(root, layout)?;
        if record::<Usage>(&path)?.is_some_and(|saved| !saved.matches(usage)) {
            return Err(invalid("cache claim address belongs to another execution"));
        }
        write_record(&path, usage)?;
        if layout == Layout::Compact {
            write_record(&project.join("current"), &key)?;
        } else {
            state_io::write_bytes(&project.join("current"), name.as_bytes()).map_err(io_error)?;
        }
        drop(selection);
        prune(root, key.project())?;
        Ok(Self {
            root: root.to_path_buf(),
            key,
            target,
            guard: Some(guard),
        })
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Err(error) = prune(&self.root, self.key.project()) {
            eprintln!("domyjob: retiring an idle build cache failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use domyjob_core::cache_key::{CacheKey, DIGEST_BYTES};

    use super::{Layout, Lease, Usage, key, release_use, state_io};
    use domyjob_core::storage_lifecycle::Quiescence;

    fn usage(root: &tempfile::TempDir, digit: &str, key: CacheKey) -> Usage {
        let store = root.path().join("runner");
        state_io::private_dir(&store).unwrap();
        let namespace = crate::process::resources::ScopeNamespace::for_store(&store).unwrap();
        Usage::for_job(
            (
                namespace,
                &domyjob_core::domain::JobId::try_from(digit.repeat(32)).unwrap(),
            ),
            key,
        )
    }

    fn initial_cache(
        root: &tempfile::TempDir,
        layout: Layout,
    ) -> (std::path::PathBuf, CacheKey, Usage, Lease) {
        let cache = root.path().join("cache");
        let key = CacheKey::new([1; DIGEST_BYTES], [2; DIGEST_BYTES]);
        let claim = usage(root, "a", key);
        let lease = match layout {
            Layout::Compact => Lease::acquire(&cache, key, &claim).unwrap(),
            Layout::Expanded | Layout::Legacy => {
                Lease::acquire_layout(&cache, key, &claim, layout).unwrap()
            }
        };
        (cache, key, claim, lease)
    }

    #[test]
    fn route_collisions_refuse_other_full_projects_or_inputs_before_usage() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let zero = [0; DIGEST_BYTES];
        let mut different = zero;
        different[DIGEST_BYTES - 1] = 1;
        let original_key = CacheKey::new(zero, zero);
        let original_claim = usage(&root, "a", original_key);
        let original = Lease::acquire(&cache, original_key, &original_claim).unwrap();
        let project = cache.join(Layout::Compact.digest(original_key.project()));
        let records = [
            project.join("project.key"),
            project.join("current"),
            original.target().parent().unwrap().join("slot.key"),
            original_claim.path(&cache, Layout::Compact).unwrap(),
        ];
        let saved = records
            .each_ref()
            .map(|path| state_io::read_bytes(path).unwrap().unwrap());
        crate::testing::write(&original.target().join("compiled"), b"original");
        for (digit, key) in [
            ("b", CacheKey::new(different, zero)),
            ("c", CacheKey::new(zero, different)),
        ] {
            assert_eq!(
                Layout::Compact.digest(original_key.project()),
                Layout::Compact.digest(key.project())
            );
            assert_eq!(
                Layout::Compact.digest(original_key.inputs()),
                Layout::Compact.digest(key.inputs())
            );
            let claim = usage(&root, digit, key);
            Lease::acquire(&cache, key, &claim).unwrap_err();
            assert_eq!(
                crate::testing::read(&original.target().join("compiled")),
                "original"
            );
            assert_eq!(
                state_io::read_bytes(&claim.path(&cache, Layout::Compact).unwrap()).unwrap(),
                None
            );
            for (path, bytes) in records.iter().zip(&saved) {
                assert_eq!(state_io::read_bytes(path).unwrap().as_ref(), Some(bytes));
            }
        }
    }

    #[test]
    fn late_old_writers_retire_after_valid_current_selection_and_confirmed_release() {
        for layout in [Layout::Expanded, Layout::Legacy] {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let old_key = CacheKey::new([1; DIGEST_BYTES], [2; DIGEST_BYTES]);
            let latest_key = CacheKey::new([1; DIGEST_BYTES], [3; DIGEST_BYTES]);
            let latest =
                Lease::acquire(&cache, latest_key, &usage(&root, "b", latest_key)).unwrap();
            let claim = usage(&root, "a", old_key);
            let old = Lease::acquire_layout(&cache, old_key, &claim, layout).unwrap();
            let target = old.target().to_path_buf();
            crate::testing::write(&target.join("compiled"), b"old");
            drop(old);
            let marker = cache.join(layout.digest(old_key.project())).join("current");
            state_io::write_bytes(&marker, layout.digest(old_key.inputs()).as_bytes()).unwrap();
            release_use(&cache, &claim, Quiescence::Unknown).unwrap();
            assert!(crate::testing::is_dir(&target));
            release_use(&cache, &claim, Quiescence::Confirmed).unwrap();
            assert!(!crate::testing::is_dir(&target));
            assert!(crate::testing::is_dir(latest.target()));
        }
    }

    #[test]
    fn copied_or_corrupt_current_selection_preserves_claims_and_existing_targets() {
        let root = tempfile::tempdir().unwrap();
        let (cache, key, claim, lease) = initial_cache(&root, Layout::Compact);
        let project = cache.join(Layout::Compact.digest(key.project()));
        let current = project.join("current");
        let original = state_io::read_bytes(&current).unwrap().unwrap();
        let usage_path = claim.path(&cache, Layout::Compact).unwrap();
        let usage_bytes = state_io::read_bytes(&usage_path).unwrap().unwrap();
        let mut foreign = key.project();
        foreign[DIGEST_BYTES - 1] ^= 1;
        let missing_key = CacheKey::new(key.project(), [3; DIGEST_BYTES]);
        let missing_slot = project
            .join("k")
            .join(Layout::Compact.digest(missing_key.inputs()));
        for invalid in [
            b"invalid selection".to_vec(),
            serde_json::to_vec(&CacheKey::new(foreign, key.inputs())).unwrap(),
            serde_json::to_vec(&missing_key).unwrap(),
        ] {
            state_io::write_bytes(&current, &invalid).unwrap();
            release_use(&cache, &claim, Quiescence::Confirmed).unwrap_err();
            assert_eq!(state_io::read_bytes(&current).unwrap().unwrap(), invalid);
            assert_eq!(
                state_io::read_bytes(&usage_path).unwrap().unwrap(),
                usage_bytes
            );
            assert!(crate::testing::is_dir(lease.target()));
            assert!(!crate::testing::is_dir(&missing_slot));
        }
        state_io::write_bytes(&current, &original).unwrap();
    }

    #[test]
    fn a_missing_current_selection_retains_old_layout_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let key = CacheKey::new([1; DIGEST_BYTES], [2; DIGEST_BYTES]);
        let modern = Lease::acquire(&cache, key, &usage(&root, "b", key)).unwrap();
        let claim = usage(&root, "a", key);
        let old = Lease::acquire_layout(&cache, key, &claim, Layout::Legacy).unwrap();
        let target = old.target().to_path_buf();
        drop(old);
        state_io::write_bytes(
            &cache
                .join(Layout::Legacy.digest(key.project()))
                .join("current"),
            Layout::Legacy.digest(key.inputs()).as_bytes(),
        )
        .unwrap();
        state_io::remove_file(
            &cache
                .join(Layout::Compact.digest(key.project()))
                .join("current"),
        )
        .unwrap();
        release_use(&cache, &claim, Quiescence::Confirmed).unwrap();
        assert!(crate::testing::is_dir(&target));
        assert!(crate::testing::is_dir(modern.target()));
    }

    #[test]
    fn malformed_old_selections_preserve_claims_and_targets_before_any_cleanup() {
        for layout in [Layout::Expanded, Layout::Legacy] {
            let root = tempfile::tempdir().unwrap();
            let (cache, key, claim, old) = initial_cache(&root, layout);
            let target = old.target().to_path_buf();
            crate::testing::write(&target.join("compiled"), b"retained");
            drop(old);
            let current = cache.join(layout.digest(key.project())).join("current");
            let claim_path = claim.path(&cache, layout).unwrap();
            let original_claim = state_io::read_bytes(&claim_path).unwrap().unwrap();
            let mut invalid_digit = layout.digest(key.inputs()).into_bytes();
            *invalid_digit.first_mut().unwrap() = b'Z';
            let mut invalid_padding = layout.digest(key.inputs()).into_bytes();
            *invalid_padding.last_mut().unwrap() = b'1';
            let mut malformed = vec![
                vec![0; invalid_digit.len()],
                invalid_digit,
                vec![0; domyjob_core::cache_key::ROUTE_BYTES],
            ];
            if layout == Layout::Expanded {
                malformed.push(invalid_padding);
            }
            for bytes in malformed {
                state_io::write_bytes(&current, &bytes).unwrap();
                release_use(&cache, &claim, Quiescence::Confirmed).unwrap_err();
                assert_eq!(state_io::read_bytes(&current).unwrap(), Some(bytes));
                assert_eq!(
                    state_io::read_bytes(&claim_path).unwrap().unwrap(),
                    original_claim
                );
                assert_eq!(crate::testing::read(&target.join("compiled")), "retained");
            }
        }
    }

    #[test]
    fn legacy_expanded_markers_require_the_matching_existing_expanded_selection() {
        let root = tempfile::tempdir().unwrap();
        let (cache, key, claim, legacy) = initial_cache(&root, Layout::Legacy);
        let legacy_target = legacy.target().to_path_buf();
        drop(legacy);
        let expanded_marker = Layout::Expanded.digest(key.inputs()).into_bytes();
        let current = cache
            .join(Layout::Legacy.digest(key.project()))
            .join("current");
        state_io::write_bytes(&current, &expanded_marker).unwrap();
        let claim_path = claim.path(&cache, Layout::Legacy).unwrap();
        let claim_bytes = state_io::read_bytes(&claim_path).unwrap().unwrap();
        release_use(&cache, &claim, Quiescence::Confirmed).unwrap_err();
        assert_eq!(
            state_io::read_bytes(&claim_path).unwrap(),
            Some(claim_bytes)
        );
        assert_eq!(
            state_io::read_bytes(&current).unwrap(),
            Some(expanded_marker)
        );
        assert!(crate::testing::is_dir(&legacy_target));
        let expanded =
            Lease::acquire_layout(&cache, key, &usage(&root, "b", key), Layout::Expanded).unwrap();
        let modern = Lease::acquire(&cache, key, &usage(&root, "c", key)).unwrap();
        drop(expanded);
        release_use(&cache, &claim, Quiescence::Confirmed).unwrap();
        assert!(!crate::testing::is_dir(&legacy_target));
        assert!(crate::testing::is_dir(modern.target()));
    }

    #[test]
    fn identical_inputs_reuse_one_slot_and_superseded_slots_wait_for_active_users() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let first_key = CacheKey::new([1; DIGEST_BYTES], [2; DIGEST_BYTES]);
        let first_use = usage(&root, "a", first_key);
        let first = Lease::acquire(&cache, first_key, &first_use).unwrap();
        let original = first.target().to_path_buf();
        crate::testing::write(&original.join("compiled"), b"reused");
        let same_use = usage(&root, "b", first_key);
        let same = Lease::acquire(&cache, first_key, &same_use).unwrap();
        assert_eq!(same.target(), original);
        assert_eq!(
            crate::testing::read(&same.target().join("compiled")),
            "reused"
        );
        drop(same);
        release_use(&cache, &same_use, Quiescence::Confirmed).unwrap();
        let replacement_key = CacheKey::new([1; DIGEST_BYTES], [3; DIGEST_BYTES]);
        let replacement =
            Lease::acquire(&cache, replacement_key, &usage(&root, "c", replacement_key)).unwrap();
        assert!(crate::testing::is_dir(&original));
        drop(first);
        release_use(&cache, &first_use, Quiescence::Unknown).unwrap();
        assert!(crate::testing::is_dir(&original));
        release_use(&cache, &first_use, Quiescence::Confirmed).unwrap();
        assert!(!crate::testing::is_dir(&original));
        assert!(crate::testing::is_dir(replacement.target()));
        assert!(
            crate::testing::read(&cache.join("CACHEDIR.TAG"))
                .starts_with("Signature: 8a477f597d28d172789f06886806bc55")
        );
    }

    #[test]
    fn a_copied_or_corrupt_cache_claim_cannot_release_another_execution() {
        let root = tempfile::tempdir().unwrap();
        let (cache, key, actual, lease) = initial_cache(&root, Layout::Compact);
        let foreign = usage(&root, "b", key);
        let path = actual.path(&cache, Layout::Compact).unwrap();
        for invalid in [
            serde_json::to_vec(&foreign).unwrap(),
            b"invalid claim".to_vec(),
        ] {
            state_io::write_bytes(&path, &invalid).unwrap();
            release_use(&cache, &actual, Quiescence::Confirmed).unwrap_err();
            assert_eq!(state_io::read_bytes(&path).unwrap(), Some(invalid));
        }
        drop(lease);
    }

    #[test]
    fn compact_selection_retires_legacy_slots_only_after_their_claims_and_leases_release() {
        let root = tempfile::tempdir().unwrap();
        let (cache, key, old_use, old) = initial_cache(&root, Layout::Legacy);
        let old_target = old.target().to_path_buf();
        crate::testing::write(&old_target.join("compiled"), b"legacy");
        let current_use = usage(&root, "b", key);
        let current = Lease::acquire(&cache, key, &current_use).unwrap();
        assert_ne!(current.target(), old_target);
        assert!(crate::testing::is_dir(&old_target));
        drop(old);
        release_use(&cache, &old_use, Quiescence::Unknown).unwrap();
        assert!(crate::testing::is_dir(&old_target));
        release_use(&cache, &old_use, Quiescence::Confirmed).unwrap();
        assert!(!crate::testing::is_dir(&old_target));
        assert!(crate::testing::is_dir(current.target()));
    }

    #[test]
    fn a_mismatched_legacy_claim_survives_compact_layout_retirement() {
        let root = tempfile::tempdir().unwrap();
        let (cache, key, actual, old) = initial_cache(&root, Layout::Legacy);
        let foreign = usage(&root, "b", key);
        let old_target = old.target().to_path_buf();
        let path = actual.path(&cache, Layout::Legacy).unwrap();
        let invalid = serde_json::to_vec(&foreign).unwrap();
        state_io::write_bytes(&path, &invalid).unwrap();
        let current = Lease::acquire(&cache, key, &usage(&root, "c", key)).unwrap();
        drop(old);
        release_use(&cache, &actual, Quiescence::Confirmed).unwrap_err();
        assert_eq!(state_io::read_bytes(&path).unwrap(), Some(invalid));
        assert!(crate::testing::is_dir(&old_target));
        assert!(crate::testing::is_dir(current.target()));
    }

    #[test]
    fn every_layout_claim_is_validated_before_any_matching_claim_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let key = CacheKey::new([1; DIGEST_BYTES], [2; DIGEST_BYTES]);
        let actual = usage(&root, "a", key);
        let foreign = usage(&root, "b", key);
        let mut leases = Vec::new();
        for layout in super::LAYOUTS.into_iter().rev() {
            leases.push(Lease::acquire_layout(&cache, key, &actual, layout).unwrap());
        }
        let legacy = actual.path(&cache, Layout::Legacy).unwrap();
        state_io::write_bytes(&legacy, &serde_json::to_vec(&foreign).unwrap()).unwrap();
        let paths = super::LAYOUTS.map(|layout| actual.path(&cache, layout).unwrap());
        let saved = paths
            .each_ref()
            .map(|path| state_io::read_bytes(path).unwrap().unwrap());
        release_use(&cache, &actual, Quiescence::Confirmed).unwrap_err();
        for (path, bytes) in paths.iter().zip(&saved) {
            assert_eq!(state_io::read_bytes(path).unwrap().as_ref(), Some(bytes));
        }
        for lease in &leases {
            assert!(crate::testing::is_dir(lease.target()));
        }
    }

    #[test]
    fn cache_keys_bind_the_archived_lockfile_and_toolchain_but_reuse_source_changes() {
        let root = tempfile::tempdir().unwrap();
        crate::testing::write(&root.path().join("Cargo.lock"), b"dependency one");
        crate::testing::write(&root.path().join("mise.toml"), b"rust one");
        crate::testing::write(&root.path().join("source.rs"), b"source one");
        let snapshot_key = || {
            let (archive, _) = crate::source::working_directory(root.path()).unwrap();
            key(root.path(), &archive).unwrap()
        };
        let original = snapshot_key();
        crate::testing::write(&root.path().join("source.rs"), b"source two");
        assert_eq!(snapshot_key(), original);
        crate::testing::write(&root.path().join("Cargo.lock"), b"dependency two");
        let changed = snapshot_key();
        assert_eq!(changed.project(), original.project());
        assert_ne!(changed.inputs(), original.inputs());
        crate::testing::write(&root.path().join("mise.toml"), b"rust two");
        assert_ne!(snapshot_key().inputs(), changed.inputs());
    }
}
