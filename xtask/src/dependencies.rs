use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Copy)]
pub enum Check {
    Deny,
    Audit,
    Vet,
}

const ADVISORY_DB_URL: &str = "https://github.com:443/RustSec/advisory-db.git";
const LOCKFILE_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct AuditedLockfile {
    sha256: String,
}

impl AuditedLockfile {
    pub(crate) fn digest(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DependencyError {
    #[error("invalid audit snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("cleaning audit snapshot {path}: {source}; audit failure: {audit:?}")]
    Cleanup {
        path: PathBuf,
        source: std::io::Error,
        audit: Option<Box<Self>>,
    },
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

fn audit_args(command: &mut Command, lock: &Path, database: &Path, fetch: bool) {
    command
        .arg("audit")
        .args(["--url", ADVISORY_DB_URL, "--db"])
        .arg(database)
        .arg("--file")
        .arg(lock);
    if !fetch {
        command.arg("--no-fetch");
    }
    command.args(["--deny", "warnings"]);
}

fn execute_check(
    command: &mut Command,
    check: &'static str,
    lock: &Path,
    execute: &mut impl FnMut(&mut Command) -> std::io::Result<ExitStatus>,
) -> Result<(), DependencyError> {
    let status = execute(command).map_err(|source| DependencyError::Start {
        check,
        lock: lock.to_path_buf(),
        source,
    })?;
    if !status.success() {
        return Err(DependencyError::Failed {
            check,
            lock: lock.to_path_buf(),
            status,
        });
    }
    Ok(())
}

fn audit_database(root: &Path) -> Result<PathBuf, DependencyError> {
    let parent = root.join("target");
    crate::raw::create_dir_all(&parent).map_err(|source| DependencyError::Prepare {
        path: parent.clone(),
        source,
    })?;
    let metadata = std::fs::symlink_metadata(&parent).map_err(|source| DependencyError::Read {
        path: parent.clone(),
        source,
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DependencyError::InvalidSnapshot(
            "audit cache parent must be a regular directory",
        ));
    }
    Ok(parent.join("advisory-db"))
}

pub(crate) fn audit_snapshot(
    root: &Path,
    lock_bytes: &[u8],
) -> Result<AuditedLockfile, DependencyError> {
    audit_snapshot_with(root, lock_bytes, Command::status)
}

fn audit_snapshot_with(
    root: &Path,
    lock_bytes: &[u8],
    mut execute: impl FnMut(&mut Command) -> std::io::Result<ExitStatus>,
) -> Result<AuditedLockfile, DependencyError> {
    if lock_bytes.is_empty() || lock_bytes.len() > LOCKFILE_LIMIT {
        return Err(DependencyError::InvalidSnapshot(
            "lockfile must be nonempty and at most 4 MiB",
        ));
    }
    std::str::from_utf8(lock_bytes)
        .map_err(|_error| DependencyError::InvalidSnapshot("lockfile must be UTF-8"))?;
    let database = audit_database(root)?;
    let directory = tempfile::Builder::new()
        .prefix(".audit-source-")
        .tempdir_in(root.join("target"))
        .map_err(|source| DependencyError::Prepare {
            path: root.join("target"),
            source,
        })?;
    let mut snapshot = tempfile::Builder::new()
        .prefix("source-")
        .suffix(".lock")
        .tempfile_in(directory.path())
        .map_err(|source| DependencyError::Prepare {
            path: directory.path().to_path_buf(),
            source,
        })?;
    let lock = snapshot.path().to_path_buf();
    let result = (|| {
        snapshot
            .write_all(lock_bytes)
            .map_err(|source| DependencyError::Prepare {
                path: lock.clone(),
                source,
            })?;
        snapshot
            .flush()
            .map_err(|source| DependencyError::Prepare {
                path: lock.clone(),
                source,
            })?;
        let mut command = crate::raw::command("cargo");
        command.current_dir(root).stdin(std::process::Stdio::null());
        for name in crate::release::SECRET_ENVIRONMENT {
            command.env_remove(name);
        }
        audit_args(&mut command, &lock, &database, true);
        execute_check(&mut command, "audit", &lock, &mut execute)?;
        let scanned = crate::release_queue::read_bounded(&lock, 4 * 1024 * 1024)
            .map_err(|_error| DependencyError::InvalidSnapshot("scanned lockfile is unsafe"))?;
        if scanned != lock_bytes {
            return Err(DependencyError::InvalidSnapshot(
                "lockfile changed during the advisory scan",
            ));
        }
        Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(lock_bytes)))
    })();
    let result = cleaned(result, &lock, snapshot.close());
    let path = directory.path().to_path_buf();
    let result = cleaned(result, &path, directory.close());
    result.map(|sha256| AuditedLockfile { sha256 })
}

fn cleaned<T>(
    result: Result<T, DependencyError>,
    path: &Path,
    cleanup: std::io::Result<()>,
) -> Result<T, DependencyError> {
    match cleanup {
        Ok(()) => result,
        Err(source) => Err(DependencyError::Cleanup {
            path: path.to_path_buf(),
            source,
            audit: result.err().map(Box::new),
        }),
    }
}

#[cfg(test)]
pub(crate) fn audited_lockfile_fixture(bytes: &[u8]) -> AuditedLockfile {
    let root = tempfile::tempdir().unwrap();
    audit_snapshot_with(root.path(), bytes, |_command| Ok(ExitStatus::default())).unwrap()
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
    let database = if matches!(check, Check::Audit) {
        audit_database(&root)?
    } else {
        root.join("target/advisory-db")
    };
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
                audit_args(&mut command, &lock, &database, index == 0);
                "audit"
            }
            Check::Vet => {
                vet_args(&mut command, &manifest, &root);
                "vet"
            }
        };
        eprintln!("{name}: {}", lock.display());
        execute_check(&mut command, name, &lock, &mut execute)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ADVISORY_DB_URL, Check, Command, DependencyError, ExitStatus, LOCKFILE_LIMIT, Path,
        PathBuf, Sha256, audit_snapshot_with, cleaned, lockfiles, lockfiles_with, manifest_beside,
        run_with,
    };
    use crate::tests::{
        ScanFailure, denied, failed_status, failing_entries, failure_path, nested_failure,
    };
    use sha2::Digest as _;

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
            unexpected @ (DependencyError::InvalidSnapshot(_)
            | DependencyError::Cleanup { .. }
            | DependencyError::Prepare { .. }
            | DependencyError::MissingRootLockfile { .. }
            | DependencyError::Start { .. }
            | DependencyError::Failed { .. }) => panic!("{unexpected}"),
        }
    }

    fn snapshot_path(command: &Command) -> PathBuf {
        command
            .get_args()
            .skip_while(|argument| *argument != "--file")
            .nth(1)
            .map(PathBuf::from)
            .unwrap()
    }

    fn assert_snapshot_removed(path: &Path) {
        assert_eq!(
            std::fs::symlink_metadata(path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            std::fs::symlink_metadata(path.parent().unwrap())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn snapshots_require_bounded_nonempty_utf8_before_starting_a_scan() {
        let root = tempfile::tempdir().unwrap();
        for input in [Vec::new(), vec![0xff], vec![b'x'; LOCKFILE_LIMIT + 1]] {
            assert!(matches!(
                audit_snapshot_with(root.path(), &input, |_command| panic!(
                    "invalid ingress cannot start an advisory scan"
                )),
                Err(DependencyError::InvalidSnapshot(_))
            ));
        }
        assert_eq!(
            std::fs::symlink_metadata(root.path().join("target"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn fresh_snapshot_scan_binds_exact_bytes_and_removes_the_private_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let bytes = include_bytes!("../../Cargo.lock");
        let mut scanned = None;
        let audited = audit_snapshot_with(root.path(), bytes, |command| {
            assert_eq!(command.get_program(), "cargo");
            assert_eq!(command.get_current_dir(), Some(root.path()));
            let lock = snapshot_path(command);
            assert_eq!(lock.extension().unwrap(), "lock");
            assert_eq!(
                crate::release_queue::read_bounded(&lock, 4 * 1024 * 1024).unwrap(),
                bytes
            );
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    "audit".as_ref(),
                    "--url".as_ref(),
                    ADVISORY_DB_URL.as_ref(),
                    "--db".as_ref(),
                    root.path().join("target").join("advisory-db").as_os_str(),
                    "--file".as_ref(),
                    lock.as_os_str(),
                    "--deny".as_ref(),
                    "warnings".as_ref(),
                ]
            );
            for name in crate::release::SECRET_ENVIRONMENT {
                assert!(
                    command
                        .get_envs()
                        .any(|(key, value)| key == name && value.is_none())
                );
            }
            scanned = Some(lock);
            Ok(ExitStatus::default())
        })
        .unwrap();
        assert_eq!(
            audited.digest(),
            data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
        );
        assert_ne!(
            audited.digest(),
            data_encoding::HEXLOWER.encode(&Sha256::digest(b"another source lockfile"))
        );
        assert_snapshot_removed(&scanned.unwrap());
    }

    #[test]
    fn parser_scan_and_process_failures_cannot_mint_capabilities_and_clean_up() {
        let root = tempfile::tempdir().unwrap();
        let failed = failed_status();
        let mut scanned = None;
        for start_failure in [false, true] {
            let error = audit_snapshot_with(root.path(), b"invalid Cargo.lock TOML", |command| {
                scanned = Some(snapshot_path(command));
                if start_failure {
                    Err(denied())
                } else {
                    Ok(failed)
                }
            })
            .unwrap_err();
            assert!(matches!(
                error,
                DependencyError::Start { .. } | DependencyError::Failed { .. }
            ));
            assert_snapshot_removed(scanned.as_ref().unwrap());
        }
    }

    #[test]
    fn snapshot_mutation_or_removal_cannot_certify_the_original_bytes() {
        let root = tempfile::tempdir().unwrap();
        for replacement in [b"changed lockfile".as_slice(), b""] {
            let mut scanned = None;
            let error =
                audit_snapshot_with(root.path(), include_bytes!("../../Cargo.lock"), |command| {
                    let lock = snapshot_path(command);
                    crate::raw::write(&lock, replacement).unwrap();
                    scanned = Some(lock);
                    Ok(ExitStatus::default())
                })
                .unwrap_err();
            assert!(matches!(error, DependencyError::InvalidSnapshot(_)));
            assert_snapshot_removed(scanned.as_ref().unwrap());
        }
    }

    #[test]
    fn cleanup_failure_cannot_return_a_digest_and_retains_the_scan_failure() {
        let path = Path::new("owned-snapshot");
        for prior_failure in [false, true] {
            let scan = if prior_failure {
                Err(DependencyError::InvalidSnapshot("scan rejected"))
            } else {
                Ok("successful scan digest")
            };
            let error = cleaned(scan, path, Err(denied())).unwrap_err();
            let DependencyError::Cleanup {
                path: actual,
                source,
                audit,
            } = error
            else {
                panic!("cleanup failure must retain its context")
            };
            assert_eq!(actual, path);
            assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(audit.is_some(), prior_failure);
            if let Some(audit) = audit {
                assert!(matches!(
                    *audit,
                    DependencyError::InvalidSnapshot("scan rejected")
                ));
            }
        }
    }
}
