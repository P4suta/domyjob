use std::path::{Path, PathBuf};
use std::process::ExitStatus;

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("listing {path}: {source}")]
    List {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} contains no workflow YAML files")]
    Empty(PathBuf),
    #[error("starting actionlint: {0}")]
    Start(std::io::Error),
    #[error("actionlint exited with {0}")]
    Failed(ExitStatus),
}

fn workflow_files(root: &Path) -> Result<Vec<PathBuf>, WorkflowError> {
    workflow_files_with(root, &crate::directory_entries)
}

fn workflow_files_with(
    root: &Path,
    scan: &impl Fn(&Path) -> crate::DirectoryEntries,
) -> Result<Vec<PathBuf>, WorkflowError> {
    let dir = root.join(".github/workflows");
    let mut files = Vec::new();
    for entry in scan(&dir).map_err(|source| WorkflowError::List {
        path: dir.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| WorkflowError::List {
            path: dir.clone(),
            source,
        })?;
        let path = entry.path;
        let kind = entry.kind.map_err(|source| WorkflowError::List {
            path: path.clone(),
            source,
        })?;
        if kind.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(WorkflowError::Empty(dir));
    }
    Ok(files)
}

pub fn check(root: &Path) -> Result<(), WorkflowError> {
    check_with(root, std::process::Command::status)
}

fn check_with(
    root: &Path,
    execute: impl FnOnce(&mut std::process::Command) -> std::io::Result<ExitStatus>,
) -> Result<(), WorkflowError> {
    let files = workflow_files(root)?;
    let mut command = crate::raw::command("actionlint");
    command.current_dir(root).args(files);
    let status = execute(&mut command).map_err(WorkflowError::Start)?;
    if status.success() {
        Ok(())
    } else {
        Err(WorkflowError::Failed(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{ScanFailure, denied, failed_status, failing_entries, failure_path};

    #[test]
    fn workflows_are_discovered_without_git_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".github/workflows");
        crate::raw::create_dir_all(&dir).unwrap();
        for name in ["a.yml", "b.yaml", "README.md"] {
            crate::raw::write(&dir.join(name), b"name: check\n").unwrap();
        }
        crate::raw::create_dir_all(&dir.join("nested.yml")).unwrap();
        crate::raw::write(&dir.join("no_extension"), b"").unwrap();
        assert_eq!(
            workflow_files(temp.path()).unwrap(),
            [dir.join("a.yml"), dir.join("b.yaml")]
        );
    }

    #[test]
    fn missing_and_empty_workflow_directories_are_errors() {
        let temp = tempfile::tempdir().unwrap();
        assert!(matches!(
            workflow_files(temp.path()),
            Err(WorkflowError::List { .. })
        ));
        let dir = temp.path().join(".github/workflows");
        crate::raw::create_dir_all(&dir).unwrap();
        crate::raw::write(&dir.join("README.md"), b"").unwrap();
        assert!(matches!(
            workflow_files(temp.path()),
            Err(WorkflowError::Empty(path)) if path == dir
        ));
    }

    #[test]
    fn workflow_discovery_preserves_entry_and_type_failures() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".github/workflows");
        for failure in [
            ScanFailure::Directory,
            ScanFailure::Entry,
            ScanFailure::Kind,
        ] {
            let error = workflow_files_with(root.path(), &|path| failing_entries(path, failure))
                .unwrap_err();
            let expected = failure_path(&dir, failure);
            assert!(matches!(error, WorkflowError::List { path, source }
                if path == expected && source.kind() == std::io::ErrorKind::PermissionDenied));
        }
    }

    #[test]
    fn actionlint_checks_every_workflow_and_reports_process_failures() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            check(root.path()),
            Err(WorkflowError::List { .. })
        ));
        let dir = root.path().join(".github/workflows");
        crate::raw::create_dir_all(&dir).unwrap();
        for name in ["a.yml", "b.yaml"] {
            crate::raw::write(&dir.join(name), b"name: fixture\n").unwrap();
        }
        check_with(root.path(), |command| {
            assert_eq!(command.get_program(), "actionlint");
            assert_eq!(command.get_current_dir(), Some(root.path()));
            assert_eq!(
                command.get_args().map(PathBuf::from).collect::<Vec<_>>(),
                [dir.join("a.yml"), dir.join("b.yaml")]
            );
            Ok(ExitStatus::default())
        })
        .unwrap();
        assert!(
            matches!(check_with(root.path(), |_| Err(denied())), Err(WorkflowError::Start(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
        let failed = failed_status();
        assert!(
            matches!(check_with(root.path(), |_| Ok(failed)), Err(WorkflowError::Failed(status)) if status == failed)
        );
    }
}
