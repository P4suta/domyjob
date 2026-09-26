use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Separator, SourceConf};
use crate::domain::{BlobId, Invalid, RelPath};
use crate::protocol::Revision;
use crate::template::{Arg, Bindings, TemplateError};

use crate::domain::METADATA_DIRS;
pub const IGNORE_FILE: &str = ".domyjobignore";
const SHOW_THREADS: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error("walking {root}: {source}")]
    Walk {
        root: PathBuf,
        source: ignore::Error,
    },
    #[error("{first} and {second} differ only in case, so they collide on macOS and Windows")]
    Collision { first: String, second: String },
    #[error("{0} cannot travel between operating systems; rename it or list it in .domyjobignore")]
    Unportable(PathBuf),
    #[error("source {source_name}: {source}")]
    Template {
        source_name: String,
        source: TemplateError,
    },
    #[error("source {source_name}: {program} could not start: {error}")]
    Start {
        source_name: String,
        program: String,
        error: std::io::Error,
    },
    #[error("source {source_name}: {command} failed: {stderr}")]
    Failed {
        source_name: String,
        command: String,
        stderr: String,
    },
    #[error("source {source_name}: {detail}")]
    Output { source_name: String, detail: String },
    #[error("{0} has no version-control source; drop @rev to send the directory as it is")]
    NoSource(PathBuf),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error("encoding the manifest: {0}")]
    Encode(serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Regular,
    Executable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    File { blob: BlobId, size: u64, mode: Mode },
    Symlink { target: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Left {
    pub path: RelPath,
    pub now: Option<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Changed {
    pub sent: u64,
    pub left: Vec<Left>,
}

impl crate::ingress::Ingress for Changed {}

fn metadata(name: &str) -> bool {
    METADATA_DIRS.contains(&name)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub entries: BTreeMap<RelPath, Entry>,
}

impl Manifest {
    pub fn check_portable(&self) -> Result<(), SnapshotError> {
        let mut seen = BTreeMap::new();
        for path in self.entries.keys() {
            let folded = path.as_str().to_lowercase();
            if let Some(first) = seen.insert(folded, path) {
                return Err(SnapshotError::Collision {
                    first: first.to_string(),
                    second: path.to_string(),
                });
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<(BlobId, Vec<u8>), SnapshotError> {
        let bytes = serde_json::to_vec(self).map_err(SnapshotError::Encode)?;
        Ok((BlobId::of(&bytes), bytes))
    }

    #[must_use]
    pub fn blobs(&self) -> Vec<BlobId> {
        let mut blobs: Vec<BlobId> = self
            .entries
            .values()
            .filter_map(|entry| match entry {
                Entry::File { blob, .. } => Some(blob.clone()),
                Entry::Symlink { .. } => None,
            })
            .collect();
        blobs.sort();
        blobs.dedup();
        blobs
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Disk(PathBuf),
    Memory(Vec<u8>),
}

impl Origin {
    pub fn read(&self) -> Result<Vec<u8>, SnapshotError> {
        match self {
            Self::Disk(path) => std::fs::read(path).map_err(|source| {
                SnapshotError::Io(crate::failure::IoFailure {
                    action: "reading",
                    path: path.clone(),
                    source,
                })
            }),
            Self::Memory(bytes) => Ok(bytes.clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub manifest: Manifest,
    pub origins: BTreeMap<BlobId, Origin>,
    pub revision: Revision,
}

pub fn relative(root: &Path, path: &Path) -> Result<RelPath, SnapshotError> {
    let unportable = || SnapshotError::Unportable(path.to_path_buf());
    let inner = path.strip_prefix(root).map_err(|_outside| unportable())?;
    let mut parts = Vec::new();
    for component in inner.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str().ok_or_else(unportable)?),
            std::path::Component::CurDir
            | std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return Err(unportable()),
        }
    }
    parts
        .join("/")
        .parse::<RelPath>()
        .map_err(|_invalid| unportable())
}

struct Pending {
    rel: RelPath,
    path: PathBuf,
    mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rules {
    Everywhere,
    InsideOnly,
}

fn walker(root: &Path, rules: Rules) -> ignore::Walk {
    let everywhere = rules == Rules::Everywhere;
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .parents(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(everywhere)
        .git_exclude(everywhere)
        .require_git(false)
        .follow_links(false)
        .add_custom_ignore_filename(IGNORE_FILE)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .is_none_or(|name| !metadata(name))
        });
    walker.build()
}

pub fn inside_paths(root: &Path) -> Result<Vec<RelPath>, SnapshotError> {
    let mut paths = Vec::new();
    for item in walker(root, Rules::InsideOnly) {
        let item = item.map_err(|source| SnapshotError::Walk {
            root: root.to_path_buf(),
            source,
        })?;
        if item.file_type().is_some_and(|kind| !kind.is_dir()) {
            paths.push(relative(root, item.path())?);
        }
    }
    Ok(paths)
}

enum Found {
    Ready(RelPath, Entry),
    Hash(Pending),
}

fn classify(root: &Path, path: &Path, symlink: bool) -> Result<Found, SnapshotError> {
    let rel = relative(root, path)?;
    let io = |action| {
        let path = path.to_path_buf();
        move |source| {
            SnapshotError::Io(crate::failure::IoFailure {
                action,
                path,
                source,
            })
        }
    };
    if symlink {
        let target = std::fs::read_link(path).map_err(io("reading link"))?;
        let target = target
            .to_str()
            .ok_or_else(|| SnapshotError::Unportable(path.to_path_buf()))?
            .to_owned();
        return Ok(Found::Ready(rel, Entry::Symlink { target }));
    }
    let meta = std::fs::symlink_metadata(path).map_err(io("reading"))?;
    Ok(Found::Hash(Pending {
        rel,
        path: path.to_path_buf(),
        mode: crate::platform::Moded::mode(&meta),
    }))
}

fn disk_origins(root: &Path, manifest: &Manifest) -> BTreeMap<BlobId, Origin> {
    let mut origins = BTreeMap::new();
    for (rel, entry) in &manifest.entries {
        if let Entry::File { blob, .. } = entry {
            origins.entry(blob.clone()).or_insert_with(|| {
                Origin::Disk(rel.parts().fold(root.to_path_buf(), |p, part| p.join(part)))
            });
        }
    }
    origins
}

pub fn from_directory(root: &Path) -> Result<Snapshot, SnapshotError> {
    let mut entries = BTreeMap::new();
    let mut pending = Vec::new();
    for item in walker(root, Rules::Everywhere) {
        let item = item.map_err(|source| SnapshotError::Walk {
            root: root.to_path_buf(),
            source,
        })?;
        let Some(kind) = item.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        match classify(root, item.path(), kind.is_symlink())? {
            Found::Ready(rel, entry) => {
                entries.insert(rel, entry);
            }
            Found::Hash(work) => pending.push(work),
        }
    }
    for (item, (blob, size)) in hash_all(&pending)? {
        entries.insert(
            item.rel.clone(),
            Entry::File {
                blob,
                size,
                mode: item.mode,
            },
        );
    }
    let manifest = Manifest { entries };
    manifest.check_portable()?;
    let origins = disk_origins(root, &manifest);
    Ok(Snapshot {
        manifest,
        origins,
        revision: Revision::WorkingDirectory,
    })
}

fn hash_file(path: &Path) -> Result<(BlobId, u64), SnapshotError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_mmap(path).map_err(|source| {
        SnapshotError::Io(crate::failure::IoFailure {
            action: "hashing",
            path: path.to_path_buf(),
            source,
        })
    })?;
    Ok((BlobId::from_hash(&hasher.finalize()), hasher.count()))
}

type Hashed<'a> = (&'a Pending, (BlobId, u64));

fn hash_all(pending: &[Pending]) -> Result<Vec<Hashed<'_>>, SnapshotError> {
    let threads = match std::thread::available_parallelism() {
        Ok(count) => count.get(),
        Err(_unknown) => 4,
    };
    let chunk = pending.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = pending
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|item| hash_file(&item.path).map(|blob| (item, blob)))
                        .collect::<Result<Vec<_>, _>>()
                })
            })
            .collect();
        let mut out = Vec::with_capacity(pending.len());
        for worker in workers {
            match worker.join() {
                Ok(done) => out.extend(done?),
                Err(_panicked) => {
                    return Err(SnapshotError::Output {
                        source_name: "directory".to_owned(),
                        detail: "a hashing thread panicked".to_owned(),
                    });
                }
            }
        }
        Ok(out)
    })
}

#[derive(Debug, Clone)]
pub struct Detected<'a> {
    pub name: &'a str,
    pub source: &'a SourceConf,
    pub root: PathBuf,
}

pub fn detect<'a>(config: &'a Config, start: &Path) -> Result<Option<Detected<'a>>, SnapshotError> {
    let sources = config.sources_by_priority();
    for dir in start.ancestors() {
        for (name, source) in &sources {
            let marker = dir.join(&source.detect);
            match std::fs::symlink_metadata(&marker) {
                Ok(_) => {
                    return Ok(Some(Detected {
                        name,
                        source,
                        root: dir.to_path_buf(),
                    }));
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(failure) => {
                    return Err(SnapshotError::Io(crate::failure::IoFailure {
                        action: "checking",
                        path: marker,
                        source: failure,
                    }));
                }
            }
        }
    }
    Ok(None)
}

fn run_template(
    (source_name, source): (&str, &SourceConf),
    argv: &[Arg],
) -> Result<Vec<u8>, SnapshotError> {
    let Some(invocation) = crate::spawn::Invocation::from_words(argv.to_vec()) else {
        return Err(SnapshotError::Output {
            source_name: source_name.to_owned(),
            detail: "empty command".to_owned(),
        });
    };
    let mut command = invocation.command();
    for name in source.unset.iter().flatten() {
        command.env_remove(name);
    }
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| SnapshotError::Start {
            source_name: source_name.to_owned(),
            program: invocation.display(),
            error,
        })?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(SnapshotError::Failed {
            source_name: source_name.to_owned(),
            command: invocation.display(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }
}

struct Listed {
    rel: RelPath,
    symlink: bool,
    mode: Mode,
}

fn parse_listing(
    source_name: &str,
    separator: Separator,
    bytes: &[u8],
) -> Result<Vec<Listed>, SnapshotError> {
    let split = match separator {
        Separator::Nul => b'\0',
        Separator::Newline => b'\n',
    };
    let bad = |detail: String| SnapshotError::Output {
        source_name: source_name.to_owned(),
        detail,
    };
    let mut out = Vec::new();
    for record in bytes.split(|byte| *byte == split) {
        let record = match separator {
            Separator::Nul => record,
            Separator::Newline => record.strip_suffix(b"\r").unwrap_or(record),
        };
        if record.is_empty() {
            continue;
        }
        let record = std::str::from_utf8(record).map_err(|_not_utf8| {
            SnapshotError::Unportable(PathBuf::from(String::from_utf8_lossy(record).into_owned()))
        })?;
        let (words, path) = match record.split_once('\t') {
            Some((words, path)) => (words, path),
            None => ("", record),
        };
        let mut words = words.split(' ');
        let (mode, kind) = match (words.next(), words.next()) {
            (Some(mode), Some(kind)) => (mode, kind),
            (Some(""), None) | (None, _) => ("", "file"),
            (Some(other), None) => {
                return Err(bad(format!(
                    "cannot read listing record {other:?} {path:?}"
                )));
            }
        };
        match kind {
            "file" | "blob" | "symlink" => {}
            "tree" | "commit" | "git-submodule" | "submodule" => continue,
            other => return Err(bad(format!("{path} is a {other}, which cannot be sent"))),
        }
        let rel = path
            .parse::<RelPath>()
            .map_err(|_invalid| SnapshotError::Unportable(PathBuf::from(path)))?;
        out.push(Listed {
            rel,
            symlink: kind == "symlink" || mode == "120000",
            mode: if mode == "true" || mode.ends_with("755") {
                Mode::Executable
            } else {
                Mode::Regular
            },
        });
    }
    Ok(out)
}

#[must_use]
pub fn identity(detected: &Detected<'_>) -> Option<PathBuf> {
    let argv = detected
        .source
        .identity
        .as_ref()?
        .render(&Bindings::new().with("root", Arg::path(&detected.root)));
    let printed = match argv.map(|argv| run_template((detected.name, detected.source), &argv)) {
        Ok(Ok(printed)) => printed,
        Ok(Err(_)) | Err(_) => return None,
    };
    let printed = String::from_utf8_lossy(&printed).trim().to_owned();
    (!printed.is_empty()).then(|| PathBuf::from(printed))
}

pub fn from_revision(
    detected: &Detected<'_>,
    rev: &crate::domain::Revision,
) -> Result<Snapshot, SnapshotError> {
    let name = detected.name;
    let source = detected.source;
    let template = |source_error| SnapshotError::Template {
        source_name: name.to_owned(),
        source: source_error,
    };
    let root = Arg::path(&detected.root);
    let resolved = run_template(
        (name, source),
        &source
            .resolve
            .render(
                &Bindings::new()
                    .with("root", root.clone())
                    .with("rev", Arg::word(rev)),
            )
            .map_err(template)?,
    )?;
    let printed = String::from_utf8_lossy(&resolved).trim().to_owned();
    let commit = crate::domain::CommitId::try_from(printed.clone()).map_err(|_not_a_commit| {
        SnapshotError::Output {
            source_name: name.to_owned(),
            detail: format!(
                "{rev} resolved to {}",
                crate::terminal::Display::of(&printed)
            ),
        }
    })?;
    let base = Bindings::new()
        .with("root", root)
        .with("commit", Arg::word(&commit));
    let listing = run_template(
        (name, source),
        &source.list.render(&base).map_err(template)?,
    )?;
    let listed = parse_listing(name, source.separator, &listing)?;
    let contents = show_all(name, source, &base, &listed)?;
    let mut entries = BTreeMap::new();
    let mut origins = BTreeMap::new();
    for (item, bytes) in listed.iter().zip(contents) {
        if item.symlink {
            let target = String::from_utf8(bytes)
                .map_err(|_binary| SnapshotError::Unportable(PathBuf::from(item.rel.as_str())))?;
            entries.insert(item.rel.clone(), Entry::Symlink { target });
        } else {
            let blob = BlobId::of(&bytes);
            let size = crate::domain::len_u64(bytes.len());
            entries.insert(
                item.rel.clone(),
                Entry::File {
                    blob: blob.clone(),
                    size,
                    mode: item.mode,
                },
            );
            origins.entry(blob).or_insert(Origin::Memory(bytes));
        }
    }
    let manifest = Manifest { entries };
    manifest.check_portable()?;
    Ok(Snapshot {
        manifest,
        origins,
        revision: Revision::Commit {
            source: name.to_owned(),
            rev: rev.to_string(),
            commit: commit.to_string(),
        },
    })
}

fn show_all(
    name: &str,
    source: &SourceConf,
    base: &Bindings,
    listed: &[Listed],
) -> Result<Vec<Vec<u8>>, SnapshotError> {
    let chunk = listed.len().div_ceil(SHOW_THREADS).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = listed
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|item| {
                            let argv = source
                                .show
                                .render(&base.clone().with("path", Arg::word(&item.rel)))
                                .map_err(|e| SnapshotError::Template {
                                    source_name: name.to_owned(),
                                    source: e,
                                })?;
                            run_template((name, source), &argv)
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
            })
            .collect();
        let mut out = Vec::with_capacity(listed.len());
        for worker in workers {
            match worker.join() {
                Ok(done) => out.extend(done?),
                Err(_panicked) => {
                    return Err(SnapshotError::Output {
                        source_name: name.to_owned(),
                        detail: "a reader thread panicked".to_owned(),
                    });
                }
            }
        }
        Ok(out)
    })
}

impl crate::ingress::Ingress for Manifest {}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build their fixtures directly on disk"
)]
mod tests {
    use super::*;

    #[expect(
        clippy::disallowed_methods,
        reason = "the test builds a fixture repository with the real git"
    )]
    fn run(dir: &Path, program: &str, args: &[&str]) {
        let no_settings = tempfile::NamedTempFile::new().unwrap();
        let out = std::process::Command::new(program)
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", no_settings.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn files(snapshot: &Snapshot) -> Vec<&str> {
        snapshot
            .manifest
            .entries
            .keys()
            .map(RelPath::as_str)
            .collect()
    }

    #[test]
    fn a_plain_directory_needs_no_version_control_and_ignores_nothing_from_outside() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*\n").unwrap();
        let root = &tmp.path().join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("target/big.bin"), "junk").unwrap();
        std::fs::write(root.join("notes.tmp"), "scratch").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join(IGNORE_FILE), "*.tmp\n").unwrap();
        let first = from_directory(root).unwrap();
        assert_eq!(
            files(&first),
            [".domyjobignore", ".gitignore", "src/main.rs"]
        );
        assert_eq!(first.revision, Revision::WorkingDirectory);
        let again = from_directory(root).unwrap();
        assert_eq!(again.manifest, first.manifest);
        std::fs::write(root.join("src/main.rs"), "fn main() { changed() }").unwrap();
        let changed = from_directory(root).unwrap();
        assert_ne!(
            changed.manifest.encode().unwrap().0,
            first.manifest.encode().unwrap().0
        );
    }

    #[test]
    fn a_clean_commit_sends_exactly_what_its_checkout_sends_whatever_the_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run(root, "git", &["init", "-q"]);
        for (name, text) in [
            ("src/\u{fc}nits/mod.rs", "nfc"),
            ("with space.txt", "space"),
            ("\u{65e5}\u{672c}\u{8a9e}/\u{6587}\u{66f8}.md", "cjk"),
            ("run.sh", "echo"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        crate::platform::set_mode(&root.join("run.sh"), Mode::Executable).unwrap();
        if crate::platform::LINKS {
            crate::platform::make_link("with space.txt", &root.join("link")).unwrap();
        }
        run(root, "git", &["add", "."]);
        run(root, "git", &["commit", "-qm", "names"]);
        let config = Config::builtin().unwrap();
        let detected = detect(&config, root).unwrap().unwrap();
        let committed = from_revision(&detected, &"HEAD".parse().unwrap()).unwrap();
        let checked_out = from_directory(root).unwrap();
        assert_eq!(committed.manifest, checked_out.manifest);
    }

    #[test]
    fn revisions_come_from_any_configured_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run(root, "git", &["init", "-q"]);
        std::fs::write(root.join("run.sh"), "echo one").unwrap();
        run(root, "git", &["add", "."]);
        run(root, "git", &["update-index", "--chmod=+x", "run.sh"]);
        run(root, "git", &["commit", "-qm", "one"]);
        std::fs::write(root.join("run.sh"), "echo two").unwrap();
        let config = Config::builtin().unwrap();
        let detected = detect(&config, root).unwrap().unwrap();
        assert_eq!(detected.name, "git");
        let snapshot = from_revision(&detected, &"HEAD".parse().unwrap()).unwrap();
        let Some(Entry::File { blob, mode, .. }) =
            snapshot.manifest.entries.get(&"run.sh".parse().unwrap())
        else {
            panic!("run.sh missing");
        };
        assert_eq!(*mode, Mode::Executable);
        assert_eq!(
            snapshot.origins.get(blob),
            Some(&Origin::Memory(b"echo one".to_vec()))
        );
        assert!(matches!(
            from_revision(&detected, &"no-such-rev".parse().unwrap()),
            Err(SnapshotError::Failed { .. })
        ));
    }

    #[test]
    fn every_worktree_of_a_repository_has_the_same_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("main");
        let side = tmp.path().join("side");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, "git", &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "a").unwrap();
        run(&root, "git", &["add", "."]);
        run(&root, "git", &["commit", "-qm", "one"]);
        run(
            &root,
            "git",
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                side.to_str().unwrap(),
            ],
        );
        let config = Config::builtin().unwrap();
        let main_id = identity(&detect(&config, &root).unwrap().unwrap()).unwrap();
        let side_id = identity(&detect(&config, &side).unwrap().unwrap()).unwrap();
        assert_eq!(
            std::fs::canonicalize(&main_id).unwrap(),
            std::fs::canonicalize(&side_id).unwrap()
        );
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        run(&other, "git", &["init", "-q"]);
        let other_id = identity(&detect(&config, &other).unwrap().unwrap()).unwrap();
        assert_ne!(
            std::fs::canonicalize(other_id).unwrap(),
            std::fs::canonicalize(main_id).unwrap()
        );
    }

    #[test]
    fn case_collisions_are_refused() {
        let blob = BlobId::of(b"x");
        let entry = || Entry::File {
            blob: blob.clone(),
            size: 1,
            mode: Mode::Regular,
        };
        let manifest = Manifest {
            entries: BTreeMap::from([
                ("Readme.md".parse().unwrap(), entry()),
                ("README.md".parse().unwrap(), entry()),
            ]),
        };
        assert!(matches!(
            manifest.check_portable(),
            Err(SnapshotError::Collision { .. })
        ));
    }

    #[test]
    fn listings_are_parsed_strictly() {
        let text = "false file\ta.txt\x00true file\tbin/x\x00false git-submodule\tsub\x00100644 blob e25f\tsp ace.txt\x00";
        let listed = parse_listing("jj", Separator::Nul, text.as_bytes()).unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed.get(1).map(|l| l.mode), Some(Mode::Executable));
        assert!(matches!(
            parse_listing("jj", Separator::Nul, b"false conflict\tx"),
            Err(SnapshotError::Output { .. })
        ));
        assert!(matches!(
            parse_listing("jj", Separator::Nul, b"false file\t../x"),
            Err(SnapshotError::Unportable(_))
        ));
        assert!(matches!(
            parse_listing("git", Separator::Nul, b"100644 blob e25f\t\xff.txt"),
            Err(SnapshotError::Unportable(_))
        ));
        let lines = parse_listing("hg", Separator::Newline, b"a.txt\r\nb.txt\n").unwrap();
        assert_eq!(lines.len(), 2);
    }
}
