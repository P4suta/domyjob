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
    let dir = root.join(".github/workflows");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|source| WorkflowError::List {
        path: dir.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| WorkflowError::List {
            path: dir.clone(),
            source,
        })?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|source| WorkflowError::List {
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
    let files = workflow_files(root)?;
    let status = crate::raw::command("actionlint")
        .current_dir(root)
        .args(files)
        .status()
        .map_err(WorkflowError::Start)?;
    if status.success() {
        Ok(())
    } else {
        Err(WorkflowError::Failed(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflows_are_discovered_without_git_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".github/workflows");
        crate::raw::create_dir_all(&dir).unwrap();
        for name in ["a.yml", "b.yaml", "README.md"] {
            crate::raw::write(&dir.join(name), b"name: check\n").unwrap();
        }
        assert_eq!(
            workflow_files(temp.path()).unwrap(),
            [dir.join("a.yml"), dir.join("b.yaml")]
        );
    }
}
