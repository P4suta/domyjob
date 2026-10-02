use std::path::{Path, PathBuf};

use syn::visit::Visit;

pub mod ci;
pub mod comments;
pub mod dependencies;
pub mod distribution;
pub mod exceptions;
pub mod fixes;
pub mod macos_package;
pub mod ownership;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "repository tasks run pinned tools and write owned caches, fixtures, and public CI outputs"
    )]

    use std::io;
    use std::path::Path;
    use std::process::Command;

    pub(super) fn command(program: &str) -> Command {
        Command::new(program)
    }

    pub(super) fn read_to_string(path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    #[cfg(test)]
    pub(super) fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(path)
    }

    pub(super) fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    pub(super) fn append(path: &Path, bytes: &[u8]) -> io::Result<()> {
        io::Write::write_all(
            &mut std::fs::OpenOptions::new().append(true).open(path)?,
            bytes,
        )
    }
}
pub mod pure_core;
pub mod release;
pub mod release_orchestration;
pub mod release_queue;
mod release_ready;
pub mod windows_contracts;
pub mod workflows;

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} does not parse: {source}")]
    Parse { path: PathBuf, source: syn::Error },
}

pub(crate) struct DirectoryEntry {
    path: PathBuf,
    name: std::ffi::OsString,
    kind: std::io::Result<EntryKind>,
}

pub(crate) struct EntryKind {
    directory: bool,
    file: bool,
}

impl EntryKind {
    pub(crate) const fn is_dir(&self) -> bool {
        self.directory
    }

    pub(crate) const fn is_file(&self) -> bool {
        self.file
    }
}

fn entry_kind(kind: std::fs::FileType) -> EntryKind {
    EntryKind {
        directory: kind.is_dir(),
        file: kind.is_file(),
    }
}

pub(crate) type DirectoryEntries = std::io::Result<Vec<std::io::Result<DirectoryEntry>>>;

pub(crate) fn directory_entries(dir: &Path) -> DirectoryEntries {
    std::fs::read_dir(dir).map(|entries| {
        entries
            .map(|entry| {
                entry.map(|entry| DirectoryEntry {
                    path: entry.path(),
                    name: entry.file_name(),
                    kind: entry.file_type().map(entry_kind),
                })
            })
            .collect()
    })
}

#[cfg(test)]
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), GateError> {
    rust_files_with(dir, out, &directory_entries)
}

fn rust_files_with(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    scan: &impl Fn(&Path) -> DirectoryEntries,
) -> Result<(), GateError> {
    for entry in scan(dir).map_err(|source| GateError::Read {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| GateError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path;
        let kind = entry.kind.map_err(|source| GateError::Read {
            path: path.clone(),
            source,
        })?;
        if kind.is_dir() {
            rust_files_with(&path, out, scan)?;
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct SourcePolicy {
    effect_module: bool,
    findings: Vec<String>,
}

impl<'ast> Visit<'ast> for SourcePolicy {
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        let line = attribute
            .path()
            .segments
            .first()
            .map_or(1, |part| part.ident.span().start().line);
        if attribute.path().is_ident("allow") {
            self.findings.push(format!(
                "{line}: use a reasoned expect attribute instead of allow"
            ));
        }
        let platform_branch = attribute.path().is_ident("cfg")
            && matches!(&attribute.meta, syn::Meta::List(list) if list.tokens.to_string() != "test")
            || attribute.path().is_ident("cfg_attr");
        if platform_branch && !self.effect_module {
            self.findings.push(format!(
                "{line}: platform branches belong in the platform or process adapter"
            ));
        }
        syn::visit::visit_attribute(self, attribute);
    }
}

fn effect_module(path: &str) -> bool {
    matches!(
        path,
        "crates/domyjob/src/platform.rs"
            | "crates/domyjob/src/process.rs"
            | "crates/domyjob/tests/e2e/os.rs"
            | "xtask/src/ci.rs"
            | "xtask/src/macos_package.rs"
            | "xtask/src/release.rs"
    ) || path.starts_with("crates/domyjob/src/platform/")
        || path.starts_with("crates/domyjob/src/process/")
        || path.starts_with("crates/domyjob/tests/e2e/os/")
}

const COMMENT: &str = "remove this comment; names and types say what the code does, and docs and commit messages say why";

#[cfg(test)]
fn read(path: &Path) -> Result<String, GateError> {
    raw::read_to_string(path).map_err(|source| GateError::Read {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn git_command() -> std::process::Command {
    let mut command = raw::command("git");
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(name);
    }
    command
}

pub(crate) fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    git_with(root, arguments, |command| {
        command
            .spawn()
            .and_then(std::process::Child::wait_with_output)
    })
}

fn git_with(
    root: &Path,
    arguments: &[&str],
    execute: impl FnOnce(&mut std::process::Command) -> std::io::Result<std::process::Output>,
) -> Result<String, String> {
    let mut command = git_command();
    command
        .current_dir(root)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = execute(&mut command).map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), GateError> {
    walk_with(root, dir, out, &directory_entries)
}

fn walk_with(
    root: &Path,
    dir: &Path,
    out: &mut Vec<String>,
    scan: &impl Fn(&Path) -> DirectoryEntries,
) -> Result<(), GateError> {
    let failed = |source| GateError::Read {
        path: dir.to_path_buf(),
        source,
    };
    for entry in scan(dir).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let name = entry.name.to_string_lossy().into_owned();
        let path = entry.path;
        let kind = entry.kind.map_err(failed)?;
        if kind.is_dir() {
            let hidden = name.starts_with('.') && name != ".github" && name != ".config";
            if !hidden && name != "target" && name != "node_modules" {
                walk_with(root, &path, out, scan)?;
            }
        } else if kind.is_file() {
            let relative = match path.strip_prefix(root) {
                Ok(relative) => relative,
                Err(_outside) => &path,
            };
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

#[cfg(test)]
fn listed(root: &Path) -> Result<Vec<String>, GateError> {
    listed_with(root, &directory_entries)
}

fn listed_with(
    root: &Path,
    scan: &impl Fn(&Path) -> DirectoryEntries,
) -> Result<Vec<String>, GateError> {
    let mut files = Vec::new();
    walk_with(root, root, &mut files, scan)?;
    files.sort();
    Ok(files)
}

#[cfg(test)]
fn present(root: &Path, file: &str) -> Result<Option<String>, GateError> {
    present_with(root, file, &raw::read_to_string)
}

fn present_with(
    root: &Path,
    file: &str,
    load: &impl Fn(&Path) -> std::io::Result<String>,
) -> Result<Option<String>, GateError> {
    let path = root.join(file);
    match load(&path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(GateError::Read { path, source }),
    }
}

fn comment_findings_with(
    root: &Path,
    scan: &impl Fn(&Path) -> DirectoryEntries,
    load: &impl Fn(&Path) -> std::io::Result<String>,
) -> Result<usize, GateError> {
    let mut findings = 0usize;
    for file in listed_with(root, scan)? {
        let extension = Path::new(&file)
            .extension()
            .map(std::ffi::OsStr::to_ascii_lowercase);
        let kind = extension.as_ref().and_then(|extension| extension.to_str());
        if !matches!(kind, Some("rs" | "toml" | "yml" | "yaml")) {
            continue;
        }
        let Some(source) = present_with(root, &file, load)? else {
            continue;
        };
        let checked = if kind == Some("rs") {
            comments::in_rust(&source)
        } else {
            comments::in_config(&file, &source)
        };
        for line in checked {
            eprintln!("{file}:{line}: {COMMENT}");
            findings = findings.saturating_add(1);
        }
    }
    Ok(findings)
}

pub fn gates(root: &Path) -> Result<usize, GateError> {
    gates_with(root, &directory_entries, &raw::read_to_string)
}

fn gates_with(
    root: &Path,
    scan: &impl Fn(&Path) -> DirectoryEntries,
    load: &impl Fn(&Path) -> std::io::Result<String>,
) -> Result<usize, GateError> {
    let mut files = Vec::new();
    for directory in ["crates", "xtask"] {
        rust_files_with(&root.join(directory), &mut files, scan)?;
    }
    files.sort();
    let mut findings = comment_findings_with(root, scan, load)?;
    for path in files {
        let source = load(&path).map_err(|source| GateError::Read {
            path: path.clone(),
            source,
        })?;
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_outside) => &path,
        };
        let shown = relative.to_string_lossy().replace('\\', "/");
        let parsed = syn::parse_file(&source).map_err(|source| GateError::Parse {
            path: path.clone(),
            source,
        })?;
        if shown.starts_with("crates/domyjob-core/src/") {
            for finding in pure_core::check(&source, shown == "crates/domyjob-core/src/lib.rs")
                .map_err(|source| GateError::Parse {
                    path: path.clone(),
                    source,
                })?
            {
                eprintln!("{shown}:{finding}");
                findings = findings.saturating_add(1);
            }
        }
        let mut policy = SourcePolicy {
            effect_module: effect_module(&shown),
            findings: Vec::new(),
        };
        policy.visit_file(&parsed);
        policy.findings.extend(exceptions::check(&shown, &parsed));
        policy.findings.extend(ownership::check(&shown, &parsed));
        for finding in policy.findings {
            eprintln!("{shown}:{finding}");
            findings = findings.saturating_add(1);
        }
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::{
        DirectoryEntries, DirectoryEntry, EntryKind, GateError, SourcePolicy,
        comment_findings_with, effect_module, gates, gates_with, listed, listed_with, present, raw,
        read, rust_files, rust_files_with, walk, walk_with,
    };
    use syn::visit::Visit;

    #[derive(Clone, Copy)]
    pub(crate) enum ScanFailure {
        Directory,
        Entry,
        Kind,
    }

    pub(crate) fn failing_entries(dir: &std::path::Path, failure: ScanFailure) -> DirectoryEntries {
        let error = denied();
        match failure {
            ScanFailure::Directory => Err(error),
            ScanFailure::Entry => Ok(vec![Err(error)]),
            ScanFailure::Kind => Ok(vec![Ok(DirectoryEntry {
                path: dir.join("entry"),
                name: "entry".into(),
                kind: Err(error),
            })]),
        }
    }

    pub(crate) fn denied() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied fixture")
    }

    pub(crate) fn failed_status() -> std::process::ExitStatus {
        super::git_command()
            .arg("--invalid-xtask-fixture")
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    pub(crate) fn failure_path(dir: &std::path::Path, failure: ScanFailure) -> std::path::PathBuf {
        if matches!(failure, ScanFailure::Kind) {
            dir.join("entry")
        } else {
            dir.to_path_buf()
        }
    }

    pub(crate) fn nested_failure(
        root: &std::path::Path,
        dir: &std::path::Path,
    ) -> DirectoryEntries {
        if dir == root {
            Ok(vec![Ok(DirectoryEntry {
                path: root.join("nested"),
                name: "nested".into(),
                kind: Ok(super::entry_kind(std::fs::metadata(root)?.file_type())),
            })])
        } else {
            failing_entries(dir, ScanFailure::Directory)
        }
    }

    #[test]
    fn platform_branches_and_allow_attributes_are_confined() {
        let source = syn::parse_file(
            "\n#[cfg(windows)] fn platform() {}\n#[allow(dead_code)] fn hidden() {}",
        )
        .unwrap();
        let mut policy = SourcePolicy::default();
        policy.visit_file(&source);
        assert_eq!(
            policy.findings,
            [
                "2: platform branches belong in the platform or process adapter",
                "3: use a reasoned expect attribute instead of allow",
            ]
        );
        let printing = syn::parse_file("\nfn f() { let output = Output::of_process(); }").unwrap();
        assert_eq!(
            super::ownership::check("crates/domyjob/src/chat/cli.rs", &printing),
            ["2: only main obtains process output"]
        );
        for path in [
            "crates/domyjob/src/platform.rs",
            "crates/domyjob/src/process.rs",
            "crates/domyjob/tests/e2e/os.rs",
            "crates/domyjob/src/platform/windows_acl.rs",
            "xtask/src/ci.rs",
            "xtask/src/macos_package.rs",
            "xtask/src/release.rs",
            "crates/domyjob/src/process/windows.rs",
            "crates/domyjob/tests/e2e/os/windows.rs",
        ] {
            assert!(effect_module(path), "{path}");
        }
        for path in [
            "crates/domyjob/src/store.rs",
            "crates/domyjob/src/platform_extra.rs",
            "crates/domyjob/tests/e2e/other.rs",
        ] {
            assert!(!effect_module(path), "{path}");
        }
    }

    #[test]
    fn platform_owners_accept_only_their_own_branches() {
        for source in [
            "#[inline] fn f() {}",
            "#[cfg(test)] mod tests {}",
            "fn f() { Output::new(); unrelated(); }",
        ] {
            let mut policy = SourcePolicy::default();
            policy.visit_file(&syn::parse_file(source).unwrap());
            assert!(policy.findings.is_empty(), "{source}");
        }
        let source = syn::parse_file(
            "#[cfg(windows)] fn f() { Output::of_process(); }\n#[cfg_attr(windows, inline)] fn g() {}",
        )
        .unwrap();
        let mut owner = SourcePolicy {
            effect_module: true,
            ..SourcePolicy::default()
        };
        owner.visit_file(&source);
        assert_eq!(owner.findings.len(), 0);
        let mut other = SourcePolicy::default();
        other.visit_file(&source);
        assert_eq!(
            other.findings,
            [
                "1: platform branches belong in the platform or process adapter",
                "2: platform branches belong in the platform or process adapter",
            ]
        );
    }

    #[test]
    fn file_discovery_includes_policy_directories_and_ignores_build_outputs() {
        let root = tempfile::tempdir().unwrap();
        for directory in [
            ".github/workflows",
            ".config",
            ".hidden",
            ".git",
            "target",
            "node_modules",
            "crates/nested",
        ] {
            let path = root.path().join(directory);
            raw::create_dir_all(&path).unwrap();
            raw::write(&path.join("fixture.rs"), b"fn f() {}").unwrap();
        }
        raw::write(&root.path().join("Cargo.toml"), b"").unwrap();
        raw::write(&root.path().join("no_extension"), b"").unwrap();
        assert_eq!(
            listed(root.path()).unwrap(),
            [
                ".config/fixture.rs",
                ".github/workflows/fixture.rs",
                "Cargo.toml",
                "crates/nested/fixture.rs",
                "no_extension",
            ]
        );
        raw::write(&root.path().join("crates/nested/README.md"), b"").unwrap();
        raw::write(&root.path().join("crates/nested/no_extension"), b"").unwrap();
        let mut sources = Vec::new();
        rust_files(&root.path().join("crates"), &mut sources).unwrap();
        assert_eq!(sources, [root.path().join("crates/nested/fixture.rs")]);
        let mut outside = Vec::new();
        walk(
            &root.path().join("elsewhere"),
            &root.path().join(".config"),
            &mut outside,
        )
        .unwrap();
        assert_eq!(
            outside,
            [root
                .path()
                .join(".config/fixture.rs")
                .to_string_lossy()
                .replace('\\', "/")]
        );
        assert!(matches!(
            rust_files(&root.path().join("missing"), &mut Vec::new()),
            Err(GateError::Read { .. })
        ));
        assert!(matches!(
            walk(root.path(), &root.path().join("missing"), &mut Vec::new()),
            Err(GateError::Read { .. })
        ));
        assert!(matches!(
            read(&root.path().join("missing")),
            Err(GateError::Read { .. })
        ));
        assert!(present(root.path(), "missing").unwrap().is_none());
        assert!(matches!(
            present(root.path(), "crates"),
            Err(GateError::Read { .. })
        ));
    }

    #[test]
    fn repository_gates_scan_sources_and_configuration_together() {
        let root = tempfile::tempdir().unwrap();
        for directory in ["crates/domyjob-core/src", "crates/domyjob/src", "xtask/src"] {
            raw::create_dir_all(&root.path().join(directory)).unwrap();
        }
        raw::write(
            &root.path().join("crates/domyjob-core/src/lib.rs"),
            b"#![no_std]\n",
        )
        .unwrap();
        raw::write(
            &root.path().join("xtask/src/lib.rs"),
            b"fn task() { std::mem::drop(()); }\n",
        )
        .unwrap();
        raw::write(&root.path().join("README.md"), b"# prose\n").unwrap();
        assert_eq!(gates(root.path()).unwrap(), 0);
        raw::write(&root.path().join("settings.TOML"), b"# setting\n").unwrap();
        raw::write(
            &root.path().join("crates/domyjob-core/src/impure.rs"),
            b"extern crate std;\n",
        )
        .unwrap();
        raw::write(
            &root.path().join("crates/domyjob/src/example.rs"),
            b"// comment\n#[allow(dead_code)] fn f() { Output::of_process(); }\n",
        )
        .unwrap();
        assert_eq!(gates(root.path()).unwrap(), 5);
        raw::write(
            &root.path().join("crates/domyjob/src/broken.rs"),
            b"fn broken(",
        )
        .unwrap();
        assert!(matches!(gates(root.path()), Err(GateError::Parse { .. })));
        assert!(matches!(
            gates(&root.path().join("missing")),
            Err(GateError::Read { .. })
        ));
    }

    #[test]
    fn discovery_preserves_directory_entry_and_type_failures() {
        let root = tempfile::tempdir().unwrap();
        for failure in [
            ScanFailure::Directory,
            ScanFailure::Entry,
            ScanFailure::Kind,
        ] {
            let scan = |dir: &std::path::Path| failing_entries(dir, failure);
            let source_error = rust_files_with(root.path(), &mut Vec::new(), &scan).unwrap_err();
            let expected = failure_path(root.path(), failure);
            assert!(matches!(source_error, GateError::Read { path, source }
                if path == expected && source.kind() == std::io::ErrorKind::PermissionDenied));
            let error = walk_with(root.path(), root.path(), &mut Vec::new(), &scan).unwrap_err();
            assert!(matches!(error, GateError::Read { path, source }
                if path == root.path() && source.kind() == std::io::ErrorKind::PermissionDenied));
        }
        let nested = root.path().join("nested");
        let scan = |dir: &std::path::Path| nested_failure(root.path(), dir);
        for error in [
            rust_files_with(root.path(), &mut Vec::new(), &scan).unwrap_err(),
            walk_with(root.path(), root.path(), &mut Vec::new(), &scan).unwrap_err(),
        ] {
            assert!(matches!(error, GateError::Read { path, source }
                if path == nested && source.kind() == std::io::ErrorKind::PermissionDenied));
        }
    }

    #[test]
    fn discovery_ignores_entries_that_are_neither_files_nor_directories() {
        let root = tempfile::tempdir().unwrap();
        let scan = |dir: &std::path::Path| {
            Ok(vec![Ok(DirectoryEntry {
                path: dir.join("link.rs"),
                name: "link.rs".into(),
                kind: Ok(EntryKind {
                    directory: false,
                    file: false,
                }),
            })])
        };
        let mut sources = Vec::new();
        rust_files_with(root.path(), &mut sources, &scan).unwrap();
        assert_eq!(sources.len(), 0);
        let mut files = Vec::new();
        walk_with(root.path(), root.path(), &mut files, &scan).unwrap();
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn repository_gate_errors_survive_each_calling_layer() {
        let root = tempfile::tempdir().unwrap();
        let scan = |dir: &std::path::Path| failing_entries(dir, ScanFailure::Directory);
        let load = |_path: &std::path::Path| Ok(String::new());
        for error in [
            listed_with(root.path(), &scan).unwrap_err(),
            comment_findings_with(root.path(), &scan, &load).unwrap_err(),
            gates_with(root.path(), &scan, &load).unwrap_err(),
        ] {
            assert!(matches!(error, GateError::Read { source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied));
        }
        for directory in ["crates", "xtask"] {
            raw::create_dir_all(&root.path().join(directory)).unwrap();
        }
        let file = root.path().join("xtask/fixture.rs");
        raw::write(&file, b"fn f() {}\n").unwrap();
        let denied = |_path: &std::path::Path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied fixture",
            ))
        };
        let error =
            comment_findings_with(root.path(), &super::directory_entries, &denied).unwrap_err();
        assert!(matches!(error, GateError::Read { path, source }
            if path == file && source.kind() == std::io::ErrorKind::PermissionDenied));
        let comment_error =
            gates_with(root.path(), &super::directory_entries, &denied).unwrap_err();
        assert!(matches!(comment_error, GateError::Read { path, source }
            if path == file && source.kind() == std::io::ErrorKind::PermissionDenied));
        let reads = std::cell::Cell::new(0);
        let replaced = |path: &std::path::Path| {
            reads.set(reads.get() + 1);
            if reads.get() == 1 {
                raw::read_to_string(path)
            } else {
                denied(path)
            }
        };
        let gate_error = gates_with(root.path(), &super::directory_entries, &replaced).unwrap_err();
        assert!(matches!(gate_error, GateError::Read { path, source }
            if path == file && source.kind() == std::io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn a_disappearing_file_does_not_hide_later_comments() {
        let root = tempfile::tempdir().unwrap();
        let disappeared = root.path().join("a.toml");
        let remaining = root.path().join("b.toml");
        raw::write(&disappeared, b"").unwrap();
        raw::write(&remaining, b"# comment\n").unwrap();
        let load = |path: &std::path::Path| {
            if path == disappeared {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "removed fixture",
                ))
            } else {
                raw::read_to_string(path)
            }
        };
        assert_eq!(
            comment_findings_with(root.path(), &super::directory_entries, &load).unwrap(),
            1
        );
    }

    #[test]
    fn git_reports_process_errors_and_preserves_failure_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let error = super::git(root.path(), &["status", "--porcelain"]).unwrap_err();
        assert!(error.contains("not a git repository"), "{error}");
        let process_error =
            super::git_with(root.path(), &["status"], |_| Err(denied())).unwrap_err();
        assert_eq!(process_error, "denied fixture");
    }

    #[test]
    fn git_child_commands_ignore_hook_repository_context() {
        if std::env::var_os("DOMYJOB_GIT_CONTEXT_PROBE").is_some() {
            let root = tempfile::tempdir().unwrap();
            super::git(root.path(), &["init", "--quiet", "--initial-branch=main"]).unwrap();
            assert!(raw::metadata(&root.path().join(".git")).unwrap().is_dir());
            raw::write(&root.path().join("fixture"), b"owned").unwrap();
            super::git(root.path(), &["add", "fixture"]).unwrap();
            assert_eq!(
                super::git(root.path(), &["config", "--get", "core.bare"])
                    .unwrap()
                    .trim(),
                "false"
            );
            return;
        }
        let foreign = tempfile::tempdir().unwrap();
        super::git(foreign.path(), &["init", "--quiet", "--bare"]).unwrap();
        let original = raw::read_to_string(&foreign.path().join("config")).unwrap();
        let child_root = tempfile::tempdir().unwrap();
        let output = raw::command(std::env::current_exe().unwrap().to_str().unwrap())
            .args([
                "--exact",
                "tests::git_child_commands_ignore_hook_repository_context",
                "--nocapture",
            ])
            .env("DOMYJOB_GIT_CONTEXT_PROBE", "1")
            .env("GIT_DIR", foreign.path())
            .env("GIT_COMMON_DIR", foreign.path())
            .env("GIT_WORK_TREE", child_root.path())
            .env("GIT_INDEX_FILE", child_root.path().join("foreign-index"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(std::process::Child::wait_with_output)
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            raw::read_to_string(&foreign.path().join("config")).unwrap(),
            original
        );
        assert!(
            matches!(raw::metadata(&child_root.path().join("foreign-index")), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn every_repository_variable_reported_by_git_is_removed_at_the_process_boundary() {
        let output = super::git_command()
            .args(["rev-parse", "--local-env-vars"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(std::process::Child::wait_with_output)
            .unwrap();
        assert!(output.status.success());
        let command = super::git_command();
        let removed = command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name.to_str().unwrap()))
            .collect::<std::collections::BTreeSet<_>>();
        for name in String::from_utf8(output.stdout).unwrap().lines() {
            assert!(
                removed.contains(name),
                "Git repository context {name} is not isolated"
            );
        }
        assert!(removed.contains("GIT_NAMESPACE"));
        assert!(raw::command("cargo").get_envs().next().is_none());
    }
}
