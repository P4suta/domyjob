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
    lockfiles_with(dir, found, &crate::directory_entries)
}

fn lockfiles_with(
    dir: &Path,
    found: &mut Vec<PathBuf>,
    scan: &impl Fn(&Path) -> crate::DirectoryEntries,
) -> Result<(), DependencyError> {
    let entries = scan(dir).map_err(|source| DependencyError::Read {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| DependencyError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = entry.name;
        let path = entry.path;
        let kind = entry.kind.map_err(|source| DependencyError::Read {
            path: path.clone(),
            source,
        })?;
        if kind.is_dir() {
            if ![".git", "target", "corpus", "artifacts", "node_modules"]
                .iter()
                .any(|ignored| name == *ignored)
            {
                lockfiles_with(&path, found, scan)?;
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

fn manifest_beside(lock: &Path) -> Result<PathBuf, DependencyError> {
    let manifest = lock.with_file_name("Cargo.toml");
    let missing = || DependencyError::MissingManifest {
        lock: lock.to_path_buf(),
    };
    match std::fs::metadata(&manifest) {
        Ok(metadata) if metadata.is_file() => Ok(manifest),
        Ok(_) => Err(missing()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(missing()),
        Err(source) => Err(DependencyError::Read {
            path: manifest,
            source,
        }),
    }
}

pub fn run(root: &Path, check: Check) -> Result<(), DependencyError> {
    run_with(root, check, Command::status)
}

fn run_with(
    root: &Path,
    check: Check,
    mut execute: impl FnMut(&mut Command) -> std::io::Result<ExitStatus>,
) -> Result<(), DependencyError> {
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
        crate::raw::create_dir_all(&database_parent).map_err(|source| {
            DependencyError::Prepare {
                path: database_parent,
                source,
            }
        })?;
    }
    for (index, lock) in locks.into_iter().enumerate() {
        let manifest = manifest_beside(&lock)?;
        let mut command = crate::raw::command("cargo");
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
        let status = execute(&mut command).map_err(|source| DependencyError::Start {
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
    use crate::tests::{
        ScanFailure, denied, failed_status, failing_entries, failure_path, nested_failure,
    };

    #[test]
    fn discovers_a_new_graph_without_a_task_list_edit() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        crate::raw::create_dir_all(&nested).unwrap();
        crate::raw::write(&root.path().join("Cargo.lock"), b"").unwrap();
        crate::raw::write(&nested.join("Cargo.lock"), b"").unwrap();
        for ignored in [".git", "target", "corpus", "artifacts", "node_modules"] {
            let dir = root.path().join(ignored);
            crate::raw::create_dir_all(&dir).unwrap();
            crate::raw::write(&dir.join("Cargo.lock"), b"").unwrap();
        }
        crate::raw::write(&root.path().join("README.md"), b"").unwrap();
        crate::raw::write(&root.path().join("Cargo.lock.backup"), b"").unwrap();
        crate::raw::write(&root.path().join("no_extension"), b"").unwrap();
        let mut found = Vec::new();
        lockfiles(root.path(), &mut found).unwrap();
        found.sort();
        assert_eq!(
            found,
            vec![root.path().join("Cargo.lock"), nested.join("Cargo.lock")]
        );
        assert!(matches!(
            lockfiles(&root.path().join("missing"), &mut Vec::new()),
            Err(DependencyError::Read { .. })
        ));
        assert!(matches!(
            manifest_beside(&root.path().join("Cargo.lock")),
            Err(DependencyError::MissingManifest { .. })
        ));
        crate::raw::create_dir_all(&root.path().join("Cargo.toml")).unwrap();
        assert!(matches!(
            manifest_beside(&root.path().join("Cargo.lock")),
            Err(DependencyError::MissingManifest { .. })
        ));
    }

    #[test]
    fn lockfile_discovery_preserves_errors_at_each_level() {
        let root = tempfile::tempdir().unwrap();
        for failure in [
            ScanFailure::Directory,
            ScanFailure::Entry,
            ScanFailure::Kind,
        ] {
            let error = lockfiles_with(root.path(), &mut Vec::new(), &|dir| {
                failing_entries(dir, failure)
            })
            .unwrap_err();
            let expected = failure_path(root.path(), failure);
            assert!(matches!(error, DependencyError::Read { path, source }
                if path == expected && source.kind() == std::io::ErrorKind::PermissionDenied));
        }
        let nested = root.path().join("nested");
        let error = lockfiles_with(root.path(), &mut Vec::new(), &|dir| {
            nested_failure(root.path(), dir)
        })
        .unwrap_err();
        assert!(matches!(error, DependencyError::Read { path, source }
            if path == nested && source.kind() == std::io::ErrorKind::PermissionDenied));
    }

    fn expected_arguments(root: &Path, check: Check, index: usize) -> Vec<String> {
        let graph = if index == 0 {
            root.to_path_buf()
        } else {
            root.join("nested")
        };
        let manifest = graph.join("Cargo.toml").to_string_lossy().into_owned();
        let lock = graph.join("Cargo.lock").to_string_lossy().into_owned();
        match check {
            Check::Deny => vec![
                "deny".into(),
                "--locked".into(),
                "--manifest-path".into(),
                manifest,
                "check".into(),
                "--hide-inclusion-graph".into(),
                "licenses".into(),
                "bans".into(),
                "sources".into(),
            ],
            Check::Audit => {
                let mut expected = vec![
                    "audit".into(),
                    "--url".into(),
                    ADVISORY_DB_URL.into(),
                    "--db".into(),
                    root.join("target/advisory-db")
                        .to_string_lossy()
                        .into_owned(),
                    "--file".into(),
                    lock,
                ];
                if index > 0 {
                    expected.push("--no-fetch".into());
                }
                expected.extend(["--deny".into(), "warnings".into()]);
                expected
            }
            Check::Vet => vec![
                "vet".into(),
                "check".into(),
                "--locked".into(),
                "--no-registry-suggestions".into(),
                "--manifest-path".into(),
                manifest,
                "--store-path".into(),
                root.join("supply-chain").to_string_lossy().into_owned(),
                "--cache-dir".into(),
                root.join("target/vet-cache").to_string_lossy().into_owned(),
            ],
        }
    }

    #[test]
    fn dependency_checks_use_each_manifest_and_share_the_root_caches() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        crate::raw::create_dir_all(&root.join("nested")).unwrap();
        for file in [
            "Cargo.lock",
            "Cargo.toml",
            "nested/Cargo.lock",
            "nested/Cargo.toml",
        ] {
            crate::raw::write(&root.join(file), b"").unwrap();
        }
        for check in [Check::Deny, Check::Audit, Check::Vet] {
            let mut commands = Vec::new();
            run_with(&root, check, |command| {
                assert_eq!(command.get_program(), "cargo");
                assert_eq!(command.get_current_dir(), Some(root.as_path()));
                commands.push(
                    command
                        .get_args()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                );
                Ok(ExitStatus::default())
            })
            .unwrap();
            assert_eq!(commands.len(), 2);
            match check {
                Check::Audit => assert!(std::fs::metadata(root.join("target")).unwrap().is_dir()),
                Check::Deny => assert!(
                    matches!(std::fs::metadata(root.join("target")), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                ),
                Check::Vet => {}
            }
            for (index, args) in commands.iter().enumerate() {
                assert_eq!(args, &expected_arguments(&root, check, index));
            }
        }
    }

    #[test]
    fn dependency_failures_keep_the_check_and_lockfile_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        crate::raw::write(&root.join("Cargo.lock"), b"").unwrap();
        crate::raw::write(&root.join("Cargo.toml"), b"").unwrap();
        for check in [Check::Deny, Check::Audit, Check::Vet] {
            let name = match check {
                Check::Deny => "deny",
                Check::Audit => "audit",
                Check::Vet => "vet",
            };
            let error = run_with(&root, check, |_| Err(denied())).unwrap_err();
            assert!(
                matches!(error, DependencyError::Start { check: failed_check, lock, source }
                if failed_check == name && lock == root.join("Cargo.lock") && source.kind() == std::io::ErrorKind::PermissionDenied)
            );
        }
        let failed = failed_status();
        let error = run_with(&root, Check::Deny, |_| Ok(failed)).unwrap_err();
        assert!(
            matches!(error, DependencyError::Failed { check: "deny", lock, status }
            if lock == root.join("Cargo.lock") && status == failed)
        );
    }

    #[test]
    fn setup_failures_do_not_start_dependency_checks() {
        let root = tempfile::tempdir().unwrap();
        let execute = |_command: &mut Command| -> std::io::Result<ExitStatus> {
            panic!("a setup error cannot run cargo")
        };
        assert!(matches!(
            run_with(&root.path().join("missing"), Check::Deny, execute),
            Err(DependencyError::Read { .. })
        ));
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(matches!(
            run_with(file.path(), Check::Deny, execute),
            Err(DependencyError::Read { .. })
        ));
        crate::raw::write(&root.path().join("Cargo.lock"), b"").unwrap();
        assert!(matches!(
            run_with(root.path(), Check::Deny, execute),
            Err(DependencyError::MissingManifest { .. })
        ));
        crate::raw::write(&root.path().join("Cargo.toml"), b"").unwrap();
        crate::raw::write(&root.path().join("target"), b"not a directory").unwrap();
        assert!(matches!(
            run_with(root.path(), Check::Audit, execute),
            Err(DependencyError::Prepare { .. })
        ));
        let lock = file.path().join("Cargo.lock");
        let kind = std::fs::metadata(lock.with_file_name("Cargo.toml"))
            .unwrap_err()
            .kind();
        match manifest_beside(&lock).unwrap_err() {
            DependencyError::MissingManifest { .. } => {
                assert_eq!(kind, std::io::ErrorKind::NotFound);
            }
            DependencyError::Read { source, .. } => assert_eq!(source.kind(), kind),
            unexpected @ (DependencyError::Prepare { .. }
            | DependencyError::MissingRootLockfile { .. }
            | DependencyError::Start { .. }
            | DependencyError::Failed { .. }) => panic!("{unexpected}"),
        }
    }
}
