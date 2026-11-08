use std::io;
use std::path::PathBuf;

use domyjob_core::domain::JobId;
use domyjob_core::scope_ownership::{Binding, NamespaceMatch, authorize};
use domyjob_core::wire::{BuildId, MAX_CONTROL_BYTES};
use serde::{Deserialize, Serialize};

use super::Scope;
use crate::state_io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScopeNamespace([u8; domyjob_core::cache_key::DIGEST_BYTES]);

impl ScopeNamespace {
    pub(crate) fn for_store(root: &std::path::Path) -> io::Result<Self> {
        std::fs::symlink_metadata(root)?;
        state_io::private_dir(root).map_err(io::Error::other)?;
        let canonical = std::fs::canonicalize(root)?;
        state_io::private_dir(&canonical).map_err(io::Error::other)?;
        Ok(Self(
            *blake3::hash(canonical.as_os_str().as_encoded_bytes()).as_bytes(),
        ))
    }

    pub(super) const fn bytes(self) -> [u8; domyjob_core::cache_key::DIGEST_BYTES] {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn specimen() -> Self {
        Self([0; domyjob_core::cache_key::DIGEST_BYTES])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedScope {
    namespace: ScopeNamespace,
    job: JobId,
    build: BuildId,
    scope: String,
}

fn corrupt() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "scope record is not bound to its store, job, and slot",
    )
}

impl SavedScope {
    pub(crate) fn new(
        (namespace, job): (ScopeNamespace, &JobId),
        build: BuildId,
        scope: &Scope,
    ) -> io::Result<Self> {
        let saved = Self {
            namespace,
            job: job.clone(),
            build,
            scope: scope.name().to_owned(),
        };
        saved.validate((namespace, job), true)?;
        Ok(saved)
    }

    pub(crate) fn validate(
        &self,
        (namespace, job): (ScopeNamespace, &JobId),
        slot_matches: bool,
    ) -> io::Result<Scope> {
        let expected = Scope::for_job((namespace, &self.job), self.build);
        if !authorize(Binding {
            job_matches: self.job == *job,
            namespace: if self.namespace == namespace {
                NamespaceMatch::Same
            } else {
                NamespaceMatch::Foreign
            },
            identity_matches: self.scope == expected.name(),
            slot_matches,
        }) {
            return Err(corrupt());
        }
        Ok(expected)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSlot {
    slot: u32,
    execution: SavedScope,
}

#[derive(Debug)]
pub(super) struct SlotRecord {
    path: PathBuf,
    slot: u32,
    namespace: ScopeNamespace,
}

impl SlotRecord {
    pub(super) const fn new(path: PathBuf, slot: u32, namespace: ScopeNamespace) -> Self {
        Self {
            path,
            slot,
            namespace,
        }
    }

    pub(super) fn write(&self, execution: &SavedScope) -> io::Result<()> {
        let saved = SavedSlot {
            slot: self.slot,
            execution: execution.clone(),
        };
        let bytes = serde_json::to_vec(&saved).map_err(io::Error::other)?;
        state_io::write_bytes(&self.path, &bytes).map_err(io::Error::other)
    }

    pub(super) fn clear_owned(&self, scope: &Scope) -> io::Result<()> {
        let Some(bytes) = state_io::read_bytes(&self.path).map_err(io::Error::other)? else {
            return Ok(());
        };
        let saved: SavedSlot =
            domyjob_core::ingress::json(&bytes, MAX_CONTROL_BYTES).map_err(|_invalid| corrupt())?;
        if saved.execution.validate(
            (self.namespace, &saved.execution.job),
            saved.slot == self.slot,
        )? != *scope
        {
            return Err(corrupt());
        }
        state_io::remove_file(&self.path).map_err(io::Error::other)
    }

    pub(super) fn clear_previous(&self) -> io::Result<bool> {
        self.clear_previous_using(
            |scope| scope.stop().and_then(|()| scope.cleanup()),
            Scope::obsolete,
        )
    }

    fn clear_previous_using(
        &self,
        stop: impl FnOnce(&Scope) -> io::Result<()>,
        obsolete: impl FnOnce(&Scope) -> io::Result<bool>,
    ) -> io::Result<bool> {
        let Some(bytes) = state_io::read_bytes(&self.path).map_err(io::Error::other)? else {
            return Ok(true);
        };
        if let Ok(saved) = domyjob_core::ingress::json::<SavedSlot>(&bytes, MAX_CONTROL_BYTES) {
            let scope = saved.execution.validate(
                (self.namespace, &saved.execution.job),
                saved.slot == self.slot,
            )?;
            stop(&scope)?;
        } else {
            let name = String::from_utf8(bytes).map_err(|_invalid| corrupt())?;
            let scope = Scope::parse(name).map_err(|_invalid| corrupt())?;
            if !obsolete(&scope)? {
                return Ok(false);
            }
        }
        state_io::remove_file(&self.path).map_err(io::Error::other)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{SavedScope, Scope, ScopeNamespace, SlotRecord};
    use domyjob_core::domain::JobId;
    use domyjob_core::wire::BuildId;

    fn execution(namespace: ScopeNamespace, digit: &str) -> SavedScope {
        let job = JobId::try_from(digit.repeat(32)).unwrap();
        let build = BuildId::from_fingerprint(42);
        SavedScope::new(
            (namespace, &job),
            build,
            &Scope::for_job((namespace, &job), build),
        )
        .unwrap()
    }

    fn record_path(root: &tempfile::TempDir) -> std::path::PathBuf {
        let directory = root.path().join("admission");
        crate::state_io::private_dir(&directory).unwrap();
        directory.join("slot.scope")
    }

    fn namespace(root: &tempfile::TempDir) -> ScopeNamespace {
        let store = root.path().join("store");
        crate::state_io::private_dir(&store).unwrap();
        ScopeNamespace::for_store(&store).unwrap()
    }

    #[test]
    fn a_slot_record_from_another_store_cannot_control_the_same_submission() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let path = record_path(&first);
        let original_namespace = namespace(&first);
        let destination_namespace = namespace(&second);
        let original = SlotRecord::new(path.clone(), 0, original_namespace);
        let destination = SlotRecord::new(path.clone(), 0, destination_namespace);
        let valid = execution(original_namespace, "a");
        let different = execution(destination_namespace, "a");
        assert_ne!(valid.scope, different.scope);
        let mut foreign_namespace = valid.clone();
        foreign_namespace.scope = different.scope;
        let mut foreign_identity = valid.clone();
        foreign_identity.namespace = destination_namespace;
        for copied in [valid, foreign_namespace, foreign_identity] {
            original.write(&copied).unwrap();
            let bytes = crate::state_io::read_bytes(&path).unwrap();
            destination
                .clear_previous_using(
                    |_scope| panic!("another store cannot stop"),
                    |_scope| panic!("typed record cannot query legacy"),
                )
                .unwrap_err();
            assert_eq!(crate::state_io::read_bytes(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn copied_slot_records_and_corrupt_identities_cannot_stop_any_scope() {
        let root = tempfile::tempdir().unwrap();
        let path = record_path(&root);
        let namespace = namespace(&root);
        let original = SlotRecord::new(path.clone(), 0, namespace);
        let foreign = SlotRecord::new(path.clone(), 1, namespace);
        let saved = execution(namespace, "a");
        original.write(&saved).unwrap();
        let bytes = crate::state_io::read_bytes(&path).unwrap();
        foreign
            .clear_previous_using(
                |_scope| panic!("copied slot must not stop"),
                |_scope| panic!("typed slot must not query legacy"),
            )
            .unwrap_err();
        assert_eq!(crate::state_io::read_bytes(&path).unwrap(), bytes);
        let mut corrupt = saved;
        corrupt.scope = execution(namespace, "b").scope;
        original.write(&corrupt).unwrap();
        original
            .clear_previous_using(
                |_scope| panic!("corrupt identity must not stop"),
                |_scope| panic!("corrupt identity must not query legacy"),
            )
            .unwrap_err();
        assert!(crate::state_io::read_bytes(&path).unwrap().is_some());
    }

    #[test]
    fn legacy_slots_only_discard_records_after_a_query_confirms_unit_absence() {
        let root = tempfile::tempdir().unwrap();
        let path = record_path(&root);
        let namespace = namespace(&root);
        let record = SlotRecord::new(path.clone(), 0, namespace);
        let scope = execution(namespace, "a")
            .validate((namespace, &JobId::try_from("a".repeat(32)).unwrap()), true)
            .unwrap();
        crate::state_io::write_bytes(&path, scope.name().as_bytes()).unwrap();
        assert!(
            !record
                .clear_previous_using(
                    |_scope| panic!("legacy records cannot stop"),
                    |queried| {
                        assert_eq!(queried, &scope);
                        Ok(false)
                    }
                )
                .unwrap()
        );
        assert!(crate::state_io::read_bytes(&path).unwrap().is_some());
        assert!(
            record
                .clear_previous_using(
                    |_scope| panic!("legacy records cannot stop"),
                    |queried| {
                        assert_eq!(queried, &scope);
                        Ok(true)
                    }
                )
                .unwrap()
        );
        assert_eq!(crate::state_io::read_bytes(&path).unwrap(), None);
        crate::state_io::write_bytes(&path, b"unknown corrupt record").unwrap();
        record
            .clear_previous_using(
                |_scope| panic!("corrupt record cannot stop"),
                |_scope| panic!("corrupt record cannot query"),
            )
            .unwrap_err();
    }

    #[test]
    fn a_verified_slot_stops_only_its_saved_build_and_then_removes_the_record() {
        let root = tempfile::tempdir().unwrap();
        let namespace = namespace(&root);
        let record = SlotRecord::new(record_path(&root), 0, namespace);
        let saved = execution(namespace, "a");
        record.write(&saved).unwrap();
        let expected = Scope::for_job((namespace, &saved.job), saved.build);
        assert!(
            record
                .clear_previous_using(
                    |scope| {
                        assert_eq!(scope, &expected);
                        Ok(())
                    },
                    |_scope| panic!("typed record must not query legacy")
                )
                .unwrap()
        );
        assert_eq!(crate::state_io::read_bytes(&record.path).unwrap(), None);
    }
}
