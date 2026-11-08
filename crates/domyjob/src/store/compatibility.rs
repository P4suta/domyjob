use std::io::ErrorKind;

use super::{JobId, Store, StoreError};

impl Store {
    pub(super) fn legacy_namespace(&self) -> bool {
        crate::formats::previous_runner() != crate::formats::runner()
            && self.root.file_name().is_some_and(|name| {
                name == format!("runner-{}", crate::formats::previous_runner()).as_str()
            })
    }

    pub(crate) fn for_job(&self, job: &JobId) -> Result<Self, StoreError> {
        let mut record = None;
        let mut completed: Option<Self> = None;
        for store in self.compatible_stores()? {
            match store.verify_job(job) {
                Ok(_) => {
                    if record.is_some() {
                        return Err(StoreError::Conflict);
                    }
                    record = Some(store.clone());
                }
                Err(StoreError::Missing) => {}
                Err(error) => return Err(error),
            }
            if store.completed_receipt(job)?.is_some() {
                if let Some(prior) = &completed
                    && !store.matching_receipt(prior, job)?
                {
                    return Err(StoreError::Conflict);
                }
                completed = Some(store);
            }
        }
        if record.is_none() && completed.is_none() {
            self.reject_unsupported_job(job)?;
        }
        if let (Some(stored), Some(_)) = (&record, &completed)
            && stored.read_state(job)?.kind() != domyjob_core::state::PhaseKind::Finished
        {
            return Err(StoreError::Conflict);
        }
        Ok(record.or(completed).unwrap_or_else(|| self.clone()))
    }

    fn reject_unsupported_job(&self, job: &JobId) -> Result<(), StoreError> {
        let Some(parent) = self.root.parent() else {
            return Ok(());
        };
        let admitted = self.compatible_stores()?;
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("runner-"))
                || admitted.iter().any(|known| known.root == entry.path())
                || !entry.file_type()?.is_dir()
            {
                continue;
            }
            match std::fs::symlink_metadata(entry.path().join("jobs").join(job.as_str())) {
                Ok(_) => return Err(StoreError::UnsupportedFormat),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) fn compatible_stores(&self) -> Result<Vec<Self>, StoreError> {
        let mut stores = vec![self.clone()];
        let Some(parent) = self.root.parent() else {
            return Ok(stores);
        };
        let current = crate::formats::runner();
        let previous = crate::formats::previous_runner();
        let admitted = [format!("runner-{current}"), format!("runner-{previous}")];
        if !self
            .root
            .file_name()
            .is_some_and(|name| admitted.iter().any(|candidate| name == candidate.as_str()))
        {
            return Ok(stores);
        }
        for name in admitted {
            let path = parent.join(name);
            if path == self.root {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    Self::verified_directory(path.clone())?;
                    stores.push(Self::open_directory(path)?);
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(stores)
    }
}

#[cfg(test)]
mod tests {
    use domyjob_core::domain::{JobId, SubmissionId};
    use domyjob_core::state::{Event, PhaseKind};

    use super::Store;

    #[test]
    fn compatible_upgrades_keep_the_original_record_and_liveness_locks() {
        const LEGACY_FORMAT: &str = "91244a79ea72e88b";
        assert_eq!(crate::formats::previous_runner(), LEGACY_FORMAT);
        let temporary = tempfile::tempdir().unwrap();
        let state = crate::layout::State::at(&temporary.path().join("state"));
        let old = Store::fixture(
            state
                .root()
                .join(format!("runner-{}", crate::formats::previous_runner())),
        )
        .unwrap();
        let current = Store::fixture(state.runner()).unwrap();
        let submission = SubmissionId::try_from("d".repeat(32)).unwrap();
        let (request, job) = crate::testing::home_job(&old, &submission);
        let workspace = old.paths(&job).workspace();
        crate::state_io::private_dir(&workspace).unwrap();
        crate::testing::write(&workspace.join("generated"), b"output");
        crate::state_io::write_bytes(&old.log_path(&job), b"finished output").unwrap();
        let alive = old.worker_lock(&job).unwrap().unwrap();
        old.transition(&job, &Event::Starting).unwrap();
        old.transition(&job, &Event::Spawned { pid: 42 }).unwrap();
        let found = current.for_job(&job).unwrap();
        assert_eq!(found.root, old.root);
        assert_eq!(found.status(&job).unwrap().kind(), PhaseKind::Running);
        assert_eq!(current.reserve(&submission, &request, None).unwrap(), job);
        assert_eq!(
            current.list().unwrap().as_slice(),
            std::slice::from_ref(&job)
        );
        old.transition(&job, &Event::Exited { code: 0 }).unwrap();
        drop(alive);
        crate::state_io::write_bytes(&old.quiescence_marker(&job).unwrap(), &[]).unwrap();
        let completion = found.wait(&job).unwrap();
        found.acknowledge(&job).unwrap();
        assert!(!crate::testing::is_dir(&workspace));
        check_consumed_record(&old, &current, &job);
        assert_eq!(
            current.for_job(&job).unwrap().status(&job).unwrap(),
            completion
        );
        assert_eq!(current.reserve(&submission, &request, None).unwrap(), job);
        let discarded = temporary.path().join("discarded");
        crate::state_io::set_aside(&old.root, &discarded).unwrap();
        assert!(crate::state_io::empty(&discarded).unwrap().is_empty());
        assert_eq!(
            current.for_job(&job).unwrap().status(&job).unwrap(),
            completion
        );
        assert_eq!(current.reserve(&submission, &request, None).unwrap(), job);
    }

    fn check_consumed_record(old: &Store, current: &Store, job: &JobId) {
        assert!(
            crate::state_io::open_read(&old.paths(job).spec())
                .unwrap()
                .is_some()
        );
        assert!(
            crate::state_io::open_read(&old.paths(job).state())
                .unwrap()
                .is_some()
        );
        assert!(
            crate::state_io::open_read(&old.paths(job).dir.join("consumed"))
                .unwrap()
                .is_some()
        );
        assert!(old.matching_receipt(current, job).unwrap());
        assert!(matches!(old.log_tail(job), Err(super::StoreError::Missing)));
        assert_eq!(current.list().unwrap(), []);
    }
}
