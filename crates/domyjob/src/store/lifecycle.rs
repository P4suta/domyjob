use domyjob_core::scope_ownership::NamespaceMatch;
use domyjob_core::state::{JobState, PhaseKind};
use domyjob_core::storage_lifecycle::{
    Completion, Delivery, Quiescence, ReceiptBinding, Retention, admit_receipt, marker_quiescence,
    retention,
};
use domyjob_core::wire::{Request, WireError};
use serde::{Deserialize, Serialize};

use super::{JobId, Store, StoreError, ingress, state_file};
use crate::lock::OsLock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    namespace: crate::process::resources::ScopeNamespace,
    job: JobId,
    request: [u8; domyjob_core::cache_key::DIGEST_BYTES],
    state: JobState,
}

impl Receipt {
    fn same_execution(&self, other: &Self) -> bool {
        self.job == other.job && self.request == other.request && self.state == other.state
    }
}

fn request_digest(
    request: &Request,
) -> Result<[u8; domyjob_core::cache_key::DIGEST_BYTES], StoreError> {
    let encoded = serde_json::to_vec(&request.canonical_submission()).map_err(WireError::from)?;
    Ok(*blake3::hash(&encoded).as_bytes())
}

impl Store {
    pub(super) fn quiescence(&self, job: &JobId) -> Result<Quiescence, StoreError> {
        let marker = self.verify_job(job)?.dir.join("quiesced");
        let record = state_file::open_read(&marker).map_err(|error| match error {
            crate::state_io::StateError::Io(_) => StoreError::State(error),
            crate::state_io::StateError::Foreign { .. }
            | crate::state_io::StateError::Exposed { .. }
            | crate::state_io::StateError::NotFile { .. }
            | crate::state_io::StateError::NotDirectory { .. }
            | crate::state_io::StateError::TooLarge { .. } => StoreError::Corrupt,
        })?;
        let size = record
            .map(|file| file.metadata().map(|metadata| metadata.len()))
            .transpose()?;
        marker_quiescence(size).map_err(|_invalid| StoreError::Corrupt)
    }

    pub(super) fn matching_receipt(&self, other: &Self, job: &JobId) -> Result<bool, StoreError> {
        Ok(match (self.receipt(job)?, other.receipt(job)?) {
            (Some(first), Some(second)) => first.same_execution(&second),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        })
    }
    pub(super) fn has_receipts(&self) -> Result<bool, StoreError> {
        match std::fs::read_dir(self.receipts_dir()) {
            Ok(mut receipts) => Ok(receipts.next().transpose()?.is_some()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(test)]
    pub(super) fn receipt_specimen(
        request: &Request,
        state: JobState,
    ) -> Result<String, StoreError> {
        let Request::Run { submission, .. } = request else {
            return Err(StoreError::Corrupt);
        };
        serde_json::to_string(&Receipt {
            namespace: crate::process::resources::ScopeNamespace::specimen(),
            job: JobId::from(submission),
            request: request_digest(request)?,
            state,
        })
        .map_err(|error| WireError::from(error).into())
    }

    #[cfg(test)]
    pub(super) fn scope_specimen(job: &JobId) -> String {
        let build = domyjob_core::wire::BuildId::from_fingerprint(42);
        let namespace = crate::process::resources::ScopeNamespace::specimen();
        let scope = crate::process::resources::Scope::for_job((namespace, job), build);
        let saved =
            crate::process::resources::SavedScope::new((namespace, job), build, &scope).unwrap();
        serde_json::to_string(&saved).unwrap()
    }

    pub(crate) fn scope_namespace(
        &self,
    ) -> Result<crate::process::resources::ScopeNamespace, StoreError> {
        Ok(crate::process::resources::ScopeNamespace::for_store(
            &self.root,
        )?)
    }

    pub(crate) fn quiescence_marker(&self, job: &JobId) -> Result<std::path::PathBuf, StoreError> {
        Ok(self.verify_job(job)?.dir.join("quiesced"))
    }

    pub(crate) fn record_scope(
        &self,
        job: &JobId,
        scope: &crate::process::resources::Scope,
    ) -> Result<(), StoreError> {
        let saved = crate::process::resources::SavedScope::new(
            (self.scope_namespace()?, job),
            crate::identity::current(),
            scope,
        )
        .map_err(|_unbound| StoreError::Corrupt)?;
        state_file::write_bytes(
            &self.verify_job(job)?.dir.join("scope"),
            &serde_json::to_vec(&saved).map_err(WireError::from)?,
        )?;
        Ok(())
    }

    pub(super) fn reconcile_scope(&self, job: &JobId) -> Result<bool, StoreError> {
        self.reconcile_scope_using(job, |scope| scope.stop().and_then(|()| scope.cleanup()))
    }

    fn reconcile_scope_using(
        &self,
        job: &JobId,
        stop: impl FnOnce(&crate::process::resources::Scope) -> std::io::Result<()>,
    ) -> Result<bool, StoreError> {
        let Some(bytes) = state_file::read_bytes(&self.paths(job).dir.join("scope"))? else {
            return Ok(false);
        };
        let saved: crate::process::resources::SavedScope =
            ingress::json(&bytes, domyjob_core::wire::MAX_CONTROL_BYTES)
                .map_err(|_invalid| StoreError::Corrupt)?;
        let scope = saved
            .validate((self.scope_namespace()?, job), true)
            .map_err(|_unbound| StoreError::Corrupt)?;
        stop(&scope)?;
        Ok(state_file::mark_existing(
            &self.paths(job).dir.join("quiesced"),
        )?)
    }

    fn receipts_dir(&self) -> std::path::PathBuf {
        self.root.join("receipts")
    }

    fn receipt(&self, job: &JobId) -> Result<Option<Receipt>, StoreError> {
        let Some(bytes) = state_file::read_bytes(&self.receipts_dir().join(job.as_str()))? else {
            return Ok(None);
        };
        let receipt: Receipt = ingress::json(&bytes, domyjob_core::wire::MAX_CONTROL_BYTES)
            .map_err(|_invalid| StoreError::Corrupt)?;
        let recorded = self.receipt_reference(job)?;
        let (request_matches, state_matches) = match recorded {
            Some((request, state)) => (
                request_digest(&request)? == receipt.request,
                state == receipt.state,
            ),
            None => (true, true),
        };
        let namespace = self.scope_namespace()?;
        if !admit_receipt(ReceiptBinding {
            namespace: if receipt.namespace == namespace {
                NamespaceMatch::Same
            } else {
                NamespaceMatch::Foreign
            },
            job_matches: receipt.job == *job,
            request_matches,
            state_matches,
            completion: if receipt.state.kind() == PhaseKind::Finished {
                Completion::Finished
            } else {
                Completion::Active
            },
        }) {
            return Err(StoreError::Corrupt);
        }
        Ok(Some(receipt))
    }

    fn receipt_reference(&self, job: &JobId) -> Result<Option<(Request, JobState)>, StoreError> {
        match self.verify_job(job) {
            Ok(_) => {}
            Err(StoreError::Missing) => return Ok(None),
            Err(error) => return Err(error),
        }
        let recorded = self
            .read_request(job)
            .and_then(|request| self.read_state(job).map(|state| (request, state)));
        match recorded {
            Ok(pair) => Ok(Some(pair)),
            Err(StoreError::Missing) => match self.verify_job(job) {
                Err(StoreError::Missing) => Ok(None),
                Ok(_) => Err(StoreError::Corrupt),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    pub(crate) fn completed_receipt(&self, job: &JobId) -> Result<Option<JobState>, StoreError> {
        Ok(self.receipt(job)?.map(|receipt| receipt.state))
    }

    pub(super) fn consumed_submission(
        &self,
        job: &JobId,
        request: &Request,
    ) -> Result<bool, StoreError> {
        match self.receipt(job)? {
            Some(receipt) => {
                super::submission_step(
                    domyjob_core::job_protocol::State::Finished,
                    receipt.request == request_digest(request)?,
                )?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub(crate) fn acknowledge(&self, job: &JobId) -> Result<(), StoreError> {
        let _admission = OsLock::exclusive(&self.admission_lock())?;
        self.record_acknowledgment(job)?;
        self.retire(job)
    }

    pub(super) fn record_acknowledgment(&self, job: &JobId) -> Result<(), StoreError> {
        if self.receipt(job)?.is_none() {
            let state = self.status(job)?;
            if state.kind() != PhaseKind::Finished {
                return Err(StoreError::Active);
            }
            let receipt = Receipt {
                namespace: self.scope_namespace()?,
                job: job.clone(),
                request: request_digest(&self.read_request(job)?)?,
                state,
            };
            state_file::private_dir(&self.receipts_dir())?;
            state_file::write_bytes(
                &self.receipts_dir().join(job.as_str()),
                &serde_json::to_vec(&receipt).map_err(WireError::from)?,
            )?;
        }
        if self.legacy_namespace() {
            let parent = self.root.parent().ok_or(StoreError::Corrupt)?;
            let current =
                Self::open_directory(parent.join(format!("runner-{}", crate::formats::runner())))?;
            let mut receipt = self.receipt(job)?.ok_or(StoreError::Corrupt)?;
            if let Some(existing) = current.receipt(job)?
                && !existing.same_execution(&receipt)
            {
                return Err(StoreError::Conflict);
            }
            receipt.namespace = current.scope_namespace()?;
            state_file::private_dir(&current.receipts_dir())?;
            state_file::write_bytes(
                &current.receipts_dir().join(job.as_str()),
                &serde_json::to_vec(&receipt).map_err(WireError::from)?,
            )?;
        }
        Ok(())
    }

    pub(crate) fn confirm_quiescence(&self, job: &JobId) -> Result<(), StoreError> {
        self.status(job)?;
        Ok(())
    }

    pub(crate) fn retire(&self, job: &JobId) -> Result<(), StoreError> {
        let paths = match self.verify_job(job) {
            Ok(paths) => paths,
            Err(StoreError::Missing) if self.receipt(job)?.is_some() => return Ok(()),
            Err(error) => return Err(error),
        };
        self.retire_paths(job, &paths)
    }

    fn retire_paths(&self, job: &JobId, paths: &super::JobPaths) -> Result<(), StoreError> {
        let Some(launch) = OsLock::try_exclusive_in_existing_directory(&paths.launch_lock())?
        else {
            return Ok(());
        };
        let Some(alive) = OsLock::try_exclusive_in_existing_directory(&paths.alive_lock())? else {
            return Ok(());
        };
        let state = match self.read_state(job) {
            Ok(state) => state,
            Err(StoreError::Missing) if self.receipt(job)?.is_some() => return Ok(()),
            Err(error) => return Err(error),
        };
        let completion = if state.kind() == PhaseKind::Finished {
            Completion::Finished
        } else {
            Completion::Active
        };
        let quiescence = self.quiescence(job)?;
        let delivery = if self.receipt(job)?.is_some() {
            Delivery::Acknowledged
        } else {
            Delivery::Pending
        };
        let retained = retention(completion, quiescence, delivery);
        if retained != Retention::WorkspaceAndResult
            && let Request::Run {
                input: domyjob_core::wire::Input::Snapshot(snapshot),
                ..
            } = self.read_request(job)?
            && let Some(key) = snapshot.cache()
        {
            let usage = crate::job_cache::Usage::for_job((self.scope_namespace()?, job), key);
            crate::job_cache::release_use(&self.cache_root()?, &usage, quiescence)?;
        }
        drop(alive);
        drop(launch);
        match retained {
            Retention::WorkspaceAndResult => {}
            Retention::Result => state_file::set_aside(&paths.workspace(), &self.trash_dir())?,
            Retention::Receipt if self.legacy_namespace() => {
                for entry in std::fs::read_dir(&paths.dir)? {
                    let entry = entry?;
                    if entry.path() == paths.spec() || entry.path() == paths.state() {
                        continue;
                    }
                    if entry.file_type()?.is_dir() {
                        state_file::set_aside(&entry.path(), &self.trash_dir())?;
                    } else {
                        state_file::remove_file(&entry.path())?;
                    }
                }
                state_file::write_bytes(&paths.dir.join("consumed"), &[])?;
            }
            Retention::Receipt => state_file::set_aside(&paths.dir, &self.trash_dir())?,
        }
        self.empty_trash();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use domyjob_core::domain::{Command, SubmissionId};
    use domyjob_core::state::Event;
    use domyjob_core::wire::{Input, Request};

    use super::{Store, StoreError};

    fn finished_result(store: &Store, digit: &str, code: i32) -> super::JobId {
        let submission = SubmissionId::try_from(digit.repeat(32)).unwrap();
        let (_, job) = crate::testing::home_job(store, &submission);
        crate::state_io::private_dir(&store.paths(&job).workspace()).unwrap();
        crate::state_io::write_bytes(&store.log_path(&job), b"unretrieved result").unwrap();
        store.transition(&job, &Event::Starting).unwrap();
        store.transition(&job, &Event::Spawned { pid: 42 }).unwrap();
        store.transition(&job, &Event::Exited { code }).unwrap();
        crate::state_io::mark_existing(&store.quiescence_marker(&job).unwrap()).unwrap();
        job
    }

    fn retained_after_invalid_receipt(store: &Store, job: &super::JobId, bytes: &[u8]) {
        crate::state_io::write_bytes(&store.receipts_dir().join(job.as_str()), bytes).unwrap();
        assert!(matches!(store.status(job), Err(StoreError::Corrupt)));
        assert!(matches!(store.acknowledge(job), Err(StoreError::Corrupt)));
        assert!(crate::testing::is_dir(&store.paths(job).dir));
        assert_eq!(
            crate::state_io::read_bytes(&store.log_path(job)).unwrap(),
            Some(b"unretrieved result".to_vec())
        );
        assert_eq!(
            crate::state_io::read_bytes(&store.receipts_dir().join(job.as_str())).unwrap(),
            Some(bytes.to_vec())
        );
    }

    #[test]
    fn copied_completion_receipts_cannot_consume_another_jobs_result() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let original = finished_result(&store, "a", 7);
        let delivered = finished_result(&store, "b", 0);
        store.acknowledge(&delivered).unwrap();
        let copied = crate::state_io::read_bytes(&store.receipts_dir().join(delivered.as_str()))
            .unwrap()
            .unwrap();
        retained_after_invalid_receipt(&store, &original, &copied);
        crate::state_io::remove_file(&store.receipts_dir().join(original.as_str())).unwrap();
        store.acknowledge(&original).unwrap();
        assert_eq!(
            store.status(&original).unwrap().command_completion(),
            Ok(Event::Exited { code: 7 })
        );
    }

    #[test]
    fn a_foreign_store_receipt_cannot_consume_the_same_submission_and_outcome() {
        let root = tempfile::tempdir().unwrap();
        let first = Store::fixture(root.path().join("first")).unwrap();
        let second = Store::fixture(root.path().join("second")).unwrap();
        let original = finished_result(&first, "a", 0);
        let foreign = finished_result(&second, "a", 0);
        assert_eq!(original, foreign);
        assert_eq!(
            first.read_request(&original).unwrap(),
            second.read_request(&foreign).unwrap()
        );
        second.acknowledge(&foreign).unwrap();
        let copied = crate::state_io::read_bytes(&second.receipts_dir().join(foreign.as_str()))
            .unwrap()
            .unwrap();
        retained_after_invalid_receipt(&first, &original, &copied);
    }

    #[test]
    fn mismatched_or_malformed_receipts_preserve_the_recorded_request_state_and_log() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let job = finished_result(&store, "a", 7);
        let valid = super::Receipt {
            namespace: store.scope_namespace().unwrap(),
            job: job.clone(),
            request: super::request_digest(&store.read_request(&job).unwrap()).unwrap(),
            state: store.read_state(&job).unwrap(),
        };
        let mut wrong_digest = valid.clone();
        wrong_digest.request = [0; domyjob_core::cache_key::DIGEST_BYTES];
        let mut wrong_state = valid.clone();
        let mut succeeded = domyjob_core::state::JobState::accepted();
        succeeded.advance(&Event::Starting).unwrap();
        succeeded.advance(&Event::Spawned { pid: 42 }).unwrap();
        succeeded.advance(&Event::Exited { code: 0 }).unwrap();
        wrong_state.state = succeeded;
        let mut active = valid.clone();
        active.state = domyjob_core::state::JobState::accepted();
        for bytes in [
            serde_json::to_vec(&wrong_digest).unwrap(),
            serde_json::to_vec(&wrong_state).unwrap(),
            serde_json::to_vec(&active).unwrap(),
            b"invalid receipt".to_vec(),
            b"{}".to_vec(),
        ] {
            retained_after_invalid_receipt(&store, &job, &bytes);
            assert_eq!(store.read_state(&job).unwrap(), valid.state);
            assert_eq!(
                super::request_digest(&store.read_request(&job).unwrap()).unwrap(),
                valid.request
            );
        }
    }

    #[test]
    fn acknowledgment_retires_bytes_without_reexecuting_the_submission() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let submission = SubmissionId::try_from("a".repeat(32)).unwrap();
        let (request, job) = crate::testing::home_job(&store, &submission);
        let workspace = store.paths(&job).workspace();
        crate::state_io::private_dir(&workspace).unwrap();
        crate::testing::write(&workspace.join("large-output"), b"generated");
        crate::state_io::write_bytes(&store.log_path(&job), b"retained result").unwrap();
        assert!(matches!(store.acknowledge(&job), Err(StoreError::Active)));
        let alive = store.worker_lock(&job).unwrap().unwrap();
        store.transition(&job, &Event::Starting).unwrap();
        store.transition(&job, &Event::Spawned { pid: 42 }).unwrap();
        let state = store.transition(&job, &Event::Exited { code: 0 }).unwrap();
        store.retire(&job).unwrap();
        assert!(crate::testing::is_dir(&workspace));
        drop(alive);
        crate::state_io::write_bytes(&store.quiescence_marker(&job).unwrap(), &[]).unwrap();
        store.confirm_quiescence(&job).unwrap();
        assert!(!crate::testing::is_dir(&workspace));
        assert_eq!(
            store.log_tail(&job).unwrap().0.for_terminal(),
            "retained result"
        );
        store.acknowledge(&job).unwrap();
        assert!(!crate::testing::is_dir(&store.paths(&job).dir));
        crate::testing::completed_job((&store, &job), &submission, &request, &state);
        store.acknowledge(&job).unwrap();
        assert!(matches!(store.log_tail(&job), Err(StoreError::Missing)));
        let conflict = Request::Run {
            submission: submission.clone(),
            command: Command::try_from(vec!["different".to_owned()]).unwrap(),
            input: Input::Home,
        };
        assert!(matches!(
            store.reserve(&submission, &conflict, None),
            Err(StoreError::Conflict)
        ));
    }

    #[test]
    fn late_retirement_does_not_recreate_a_consumed_job_or_lose_its_receipt() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let submission = SubmissionId::try_from("e".repeat(32)).unwrap();
        let (request, job) = crate::testing::home_job(&store, &submission);
        store.transition(&job, &Event::Killed).unwrap();
        let observed = store.verify_job(&job).unwrap();
        let completion = store.status(&job).unwrap();
        store.acknowledge(&job).unwrap();
        store.retire_paths(&job, &observed).unwrap();
        assert_eq!(
            std::fs::metadata(observed.dir).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        crate::testing::completed_job((&store, &job), &submission, &request, &completion);
    }

    #[test]
    fn lost_execution_without_quiescence_retains_its_workspace_after_acknowledgment() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let submission = SubmissionId::try_from("b".repeat(32)).unwrap();
        let (request, job) = crate::testing::home_job(&store, &submission);
        let workspace = store.paths(&job).workspace();
        crate::state_io::private_dir(&workspace).unwrap();
        crate::testing::write(&workspace.join("owned"), b"keep while uncertain");
        store.transition(&job, &Event::Starting).unwrap();
        store.transition(&job, &Event::Spawned { pid: 42 }).unwrap();
        store.status(&job).unwrap();
        store.acknowledge(&job).unwrap();
        assert!(crate::testing::is_dir(&workspace));
        crate::state_io::mark_existing(&store.quiescence_marker(&job).unwrap()).unwrap();
        let completion = store.wait(&job).unwrap();
        assert!(!crate::testing::is_dir(&workspace));
        crate::testing::completed_job((&store, &job), &submission, &request, &completion);
    }

    #[test]
    fn unavailable_receipt_storage_preserves_results_until_acknowledgment_can_retry() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let submission = SubmissionId::try_from("c".repeat(32)).unwrap();
        let (request, job) = crate::testing::home_job(&store, &submission);
        let paths = store.paths(&job);
        crate::state_io::private_dir(&paths.workspace()).unwrap();
        crate::state_io::write_bytes(&paths.log(), b"undelivered diagnostic").unwrap();
        store
            .finish_launch_failure(
                &job,
                domyjob_core::domain::RemoteText::try_from("launch failed".to_owned()).unwrap(),
            )
            .unwrap();
        let completion = store.read_state(&job).unwrap();
        let obstructed = store.receipts_dir();
        crate::state_io::write_bytes(&obstructed, b"receipt storage unavailable").unwrap();
        store.acknowledge(&job).unwrap_err();
        assert!(crate::testing::is_dir(&paths.dir));
        assert_eq!(store.read_state(&job).unwrap(), completion);
        assert_eq!(
            crate::state_io::read_bytes(&paths.log()).unwrap(),
            Some(b"undelivered diagnostic".to_vec())
        );
        crate::state_io::remove_file(&obstructed).unwrap();
        assert_eq!(
            store.log_tail(&job).unwrap().0.for_terminal(),
            "undelivered diagnostic"
        );
        assert_eq!(store.reserve(&submission, &request, None).unwrap(), job);
        store.acknowledge(&job).unwrap();
        assert!(!crate::testing::is_dir(&paths.dir));
        crate::testing::completed_job((&store, &job), &submission, &request, &completion);
    }

    #[test]
    fn nonempty_confirmation_markers_cannot_retire_job_data() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let submission = SubmissionId::try_from("d".repeat(32)).unwrap();
        let (_, job) = crate::testing::home_job(&store, &submission);
        let paths = store.paths(&job);
        crate::state_io::private_dir(&paths.workspace()).unwrap();
        crate::state_io::write_bytes(&paths.log(), b"retained output").unwrap();
        store.transition(&job, &Event::Killed).unwrap();
        let marker = store.quiescence_marker(&job).unwrap();
        crate::state_io::write_bytes(&marker, b"invalid completion record").unwrap();
        assert!(matches!(store.status(&job), Err(StoreError::Corrupt)));
        assert!(matches!(store.acknowledge(&job), Err(StoreError::Corrupt)));
        assert!(crate::testing::is_dir(&paths.workspace()));
        assert_eq!(
            crate::state_io::read_bytes(&paths.log()).unwrap(),
            Some(b"retained output".to_vec())
        );
        assert_eq!(store.completed_receipt(&job).unwrap(), None);
        crate::state_io::remove_file(&marker).unwrap();
        assert!(crate::state_io::mark_existing(&marker).unwrap());
        let completion = store.status(&job).unwrap();
        assert!(!crate::testing::is_dir(&paths.workspace()));
        store.acknowledge(&job).unwrap();
        assert!(!crate::testing::is_dir(&paths.dir));
        assert_eq!(store.status(&job).unwrap(), completion);
    }

    #[test]
    fn copied_scope_records_from_another_store_cannot_control_the_same_submission() {
        use crate::process::resources::{SavedScope, Scope};
        use domyjob_core::wire::BuildId;

        let root = tempfile::tempdir().unwrap();
        let original = Store::fixture(root.path().join("original")).unwrap();
        let destination = Store::fixture(root.path().join("destination")).unwrap();
        let submission = SubmissionId::try_from("d".repeat(32)).unwrap();
        let (request, job) = crate::testing::home_job(&original, &submission);
        assert_eq!(
            destination.reserve(&submission, &request, None).unwrap(),
            job
        );
        let build = BuildId::from_fingerprint(42);
        let original_namespace = original.scope_namespace().unwrap();
        let destination_namespace = destination.scope_namespace().unwrap();
        assert_eq!(
            original_namespace,
            crate::process::resources::ScopeNamespace::for_store(&original.root.join(".")).unwrap()
        );
        let scope = Scope::for_job((original_namespace, &job), build);
        assert_ne!(Scope::for_job((destination_namespace, &job), build), scope);
        let copied = serde_json::to_vec(
            &SavedScope::new((original_namespace, &job), build, &scope).unwrap(),
        )
        .unwrap();
        let path = destination.paths(&job).dir.join("scope");
        crate::state_io::write_bytes(&path, &copied).unwrap();
        assert!(matches!(
            destination.reconcile_scope_using(&job, |_scope| panic!("another store cannot stop")),
            Err(StoreError::Corrupt)
        ));
        assert_eq!(crate::state_io::read_bytes(&path).unwrap(), Some(copied));
        assert_eq!(
            destination.quiescence(&job).unwrap(),
            domyjob_core::storage_lifecycle::Quiescence::Unknown
        );
    }

    #[test]
    fn corrupt_or_foreign_scope_records_fail_before_stopping_any_job() {
        use crate::process::resources::{SavedScope, Scope};
        use domyjob_core::wire::BuildId;
        let root = tempfile::tempdir().unwrap();
        let store = Store::fixture(root.path().join("runner")).unwrap();
        let submission = SubmissionId::try_from("e".repeat(32)).unwrap();
        let (_, job) = crate::testing::home_job(&store, &submission);
        let other = super::JobId::try_from("f".repeat(32)).unwrap();
        let build = BuildId::from_fingerprint(42);
        let namespace = store.scope_namespace().unwrap();
        let scope = Scope::for_job((namespace, &job), build);
        let foreign = Scope::for_job((namespace, &other), build);
        let mut wrong_identity =
            serde_json::to_value(SavedScope::new((namespace, &job), build, &scope).unwrap())
                .unwrap();
        *wrong_identity.get_mut("scope").unwrap() =
            serde_json::Value::String(foreign.name().to_owned());
        let path = store.paths(&job).dir.join("scope");
        let workspace = store.paths(&job).workspace();
        crate::state_io::private_dir(&workspace).unwrap();
        for invalid in [
            foreign.name().as_bytes().to_vec(),
            serde_json::to_vec(&SavedScope::new((namespace, &other), build, &foreign).unwrap())
                .unwrap(),
            serde_json::to_vec(&wrong_identity).unwrap(),
            b"invalid scope record".to_vec(),
        ] {
            crate::state_io::write_bytes(&path, &invalid).unwrap();
            assert!(matches!(
                store.reconcile_scope_using(&job, |_scope| panic!("unbound scope cannot stop")),
                Err(StoreError::Corrupt)
            ));
            assert!(crate::testing::is_dir(&workspace));
            assert_eq!(crate::state_io::read_bytes(&path).unwrap(), Some(invalid));
            assert_eq!(
                crate::state_io::read_bytes(&store.quiescence_marker(&job).unwrap()).unwrap(),
                None
            );
        }
        let saved = SavedScope::new((namespace, &job), build, &scope).unwrap();
        crate::state_io::write_bytes(&path, &serde_json::to_vec(&saved).unwrap()).unwrap();
        assert!(
            store
                .reconcile_scope_using(&job, |observed| {
                    assert_eq!(observed, &scope);
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!(
            store.quiescence(&job).unwrap(),
            domyjob_core::storage_lifecycle::Quiescence::Confirmed
        );
    }
}
