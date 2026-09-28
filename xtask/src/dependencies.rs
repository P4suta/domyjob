use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

#[derive(Debug, Clone, Copy)]
pub enum Check {
    Deny,
    Audit,
    Vet,
}

const ADVISORY_DB_URL: &str = "https://github.com:443/RustSec/advisory-db.git";

#[derive(Debug, thiserror::Error)]
pub enum DependencyError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("preparing {path}: {source}")]
    Prepare {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the root Cargo.lock is missing under {root}")]
    MissingRootLockfile { root: PathBuf },
    #[error("{lock} has no adjacent Cargo.toml")]
    MissingManifest { lock: PathBuf },
    #[error("running {check} for {lock}: {source}")]
    Start {
        check: &'static str,
        lock: PathBuf,
        source: std::io::Error,
    },
    #[error("{check} failed for {lock}: {status}")]
    Failed {
        check: &'static str,
        lock: PathBuf,
        status: ExitStatus,
    },
}

fn lockfiles(dir: &Path, found: &mut Vec<PathBuf>) -> Result<(), DependencyError> {
    let entries = std::fs::read_dir(dir).map_err(|source| DependencyError::Read {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| DependencyError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let path = entry.path();
        let kind = entry.file_type().map_err(|source| DependencyError::Read {
            path: path.clone(),
            source,
        })?;
        if kind.is_dir() {
            if ![".git", "target", "corpus", "artifacts", "node_modules"]
                .iter()
                .any(|ignored| name == *ignored)
            {
                lockfiles(&path, found)?;
            }
        } else if kind.is_file() && name == "Cargo.lock" {
            found.push(path);
        }
    }
    Ok(())
}

fn vet_args(command: &mut Command, manifest: &Path, root: &Path) {
    command
        .args(["vet", "check", "--locked", "--no-registry-suggestions"])
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--store-path")
        .arg(root.join("supply-chain"))
        .arg("--cache-dir")
        .arg(root.join("target/vet-cache"));
}

#[expect(
    clippy::disallowed_methods,
    reason = "the repository task launches Cargo checks for every dependency graph"
)]
pub fn run(root: &Path, check: Check) -> Result<(), DependencyError> {
    let root = root
        .canonicalize()
        .map_err(|source| DependencyError::Read {
            path: root.to_path_buf(),
            source,
        })?;
    let mut locks = Vec::new();
    lockfiles(&root, &mut locks)?;
    locks.sort();
    if !locks.contains(&root.join("Cargo.lock")) {
        return Err(DependencyError::MissingRootLockfile { root });
    }
    let database_parent = root.join("target");
    let database = database_parent.join("advisory-db");
    if matches!(check, Check::Audit) {
        std::fs::create_dir_all(&database_parent).map_err(|source| DependencyError::Prepare {
            path: database_parent,
            source,
        })?;
    }
    for (index, lock) in locks.into_iter().enumerate() {
        let manifest = lock.with_file_name("Cargo.toml");
        if !manifest.is_file() {
            return Err(DependencyError::MissingManifest { lock });
        }
        let mut command = Command::new("cargo");
        command.current_dir(&root);
        let name = match check {
            Check::Deny => {
                command
                    .args(["deny", "--locked", "--manifest-path"])
                    .arg(&manifest)
                    .args([
                        "check",
                        "--hide-inclusion-graph",
                        "licenses",
                        "bans",
                        "sources",
                    ]);
                "deny"
            }
            Check::Audit => {
                command
                    .arg("audit")
                    .args(["--url", ADVISORY_DB_URL, "--db"])
                    .arg(&database)
                    .arg("--file")
                    .arg(&lock);
                if index > 0 {
                    command.arg("--no-fetch");
                }
                command.args(["--deny", "warnings"]);
                "audit"
            }
            Check::Vet => {
                vet_args(&mut command, &manifest, &root);
                "vet"
            }
        };
        eprintln!("{name}: {}", lock.display());
        let status = command.status().map_err(|source| DependencyError::Start {
            check: name,
            lock: lock.clone(),
            source,
        })?;
        if !status.success() {
            return Err(DependencyError::Failed {
                check: name,
                lock,
                status,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::disallowed_methods,
        reason = "the test creates a dependency graph fixture outside production state"
    )]
    #[test]
    fn discovers_a_new_graph_without_a_task_list_edit() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.path().join("Cargo.lock"), "").unwrap();
        std::fs::write(nested.join("Cargo.lock"), "").unwrap();
        std::fs::create_dir_all(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/Cargo.lock"), "").unwrap();
        let mut found = Vec::new();
        lockfiles(root.path(), &mut found).unwrap();
        found.sort();
        assert_eq!(
            found,
            vec![root.path().join("Cargo.lock"), nested.join("Cargo.lock")]
        );
    }
}
