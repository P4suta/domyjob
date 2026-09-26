use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cas::{Applied, Cas, CasError};
use crate::domain::RelPath;
use crate::snapshot::{Entry, Left, Manifest};
use crate::tree::{Blockers, Contents, Placed, Removed, Rooted, TreeError};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error("the workspace was left half-updated because the job was stopped")]
    Stopped,
    #[error("{0} is a directory; get copies one file, so name a file inside it")]
    NotAFile(PathBuf),
    #[error("the workspace {0} is gone")]
    Gone(PathBuf),
    #[error(transparent)]
    Snapshot(#[from] crate::snapshot::SnapshotError),
}

#[derive(Debug)]
pub struct Workspace(Rooted);

#[derive(Debug, Clone, Copy)]
pub struct Plan<'a> {
    pub manifest: &'a Manifest,
    pub previous: &'a Applied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Changes {
    pub written: usize,
    pub kept: usize,
    pub removed: usize,
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self, WorkspaceError> {
        Self::open_as(root, crate::platform::FAMILY)
    }

    pub fn open_as(root: &Path, family: crate::paths::Family) -> Result<Self, WorkspaceError> {
        crate::state_file::private_dir(root)?;
        Rooted::open_as(root, family)?
            .map(Self)
            .ok_or_else(|| WorkspaceError::Gone(root.to_path_buf()))
    }

    pub fn open_existing(root: &Path) -> Result<Option<Self>, WorkspaceError> {
        Ok(Rooted::open(root)?.map(Self))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.0.root()
    }

    fn holds(&self, rel: &RelPath, entry: &Entry) -> Result<bool, WorkspaceError> {
        Ok(self.0.holds(rel, Some(entry), &Removed::new())?)
    }

    fn place(&self, cas: &Cas, rel: &RelPath, entry: &Entry) -> Result<(), WorkspaceError> {
        self.0.make_parents(rel, Blockers::Replace)?;
        self.0.clear(rel, Contents::Anything)?;
        match entry {
            Entry::File { blob, mode, .. } => {
                self.0.create(rel, Placed::File(&cas.get(blob)?, *mode))?;
            }
            Entry::Symlink { target } => self.0.create(rel, Placed::Link(target))?,
        }
        Ok(())
    }

    pub fn materialize(
        &self,
        cas: &Cas,
        plan: Plan<'_>,
        stop: &AtomicBool,
    ) -> Result<(Applied, Changes), WorkspaceError> {
        let Plan { manifest, previous } = plan;
        let mut changes = Changes::default();
        for rel in previous
            .paths()
            .filter(|rel| !manifest.entries.contains_key(*rel))
        {
            self.0.clear(rel, Contents::Anything)?;
            self.0.prune(rel);
            changes.removed = changes.removed.saturating_add(1);
        }
        let mut next = Applied::default();
        for (rel, entry) in &manifest.entries {
            if stop.load(Ordering::SeqCst) {
                return Err(WorkspaceError::Stopped);
            }
            if self.holds(rel, entry)? {
                changes.kept = changes.kept.saturating_add(1);
            } else {
                self.place(cas, rel, entry)?;
                changes.written = changes.written.saturating_add(1);
            }
            next.insert(rel.clone());
        }
        Ok((next, changes))
    }

    pub fn left(&self, sent: &Manifest) -> Result<Vec<Left>, WorkspaceError> {
        let mut left = Vec::new();
        for (rel, entry) in &sent.entries {
            if !self.holds(rel, entry)? {
                left.push(Left {
                    path: rel.clone(),
                    now: self.0.entry(rel, Some(entry))?,
                });
            }
        }
        for rel in crate::snapshot::inside_paths(self.root())? {
            if !sent.entries.contains_key(&rel)
                && let Some(now) = self.0.entry(&rel, None)?
            {
                left.push(Left {
                    path: rel,
                    now: Some(now),
                });
            }
        }
        left.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(left)
    }

    pub fn open_file(&self, rel: &RelPath) -> Result<std::fs::File, WorkspaceError> {
        self.0
            .open_file(rel)?
            .ok_or_else(|| WorkspaceError::NotAFile(rel.to_local()))
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build their fixtures directly on disk"
)]
mod tests {
    use super::*;
    use crate::snapshot::Mode;
    use crate::snapshot::from_directory;
    use std::io::ErrorKind;

    #[test]
    fn a_fill_cut_short_leaves_nothing_behind_once_its_intent_was_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("kept.rs"), "kept").unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let next = from_directory(&src).unwrap();
        for blob in cas.missing(&next.manifest.blobs()).unwrap() {
            cas.put(&blob, &next.origins.get(&blob).unwrap().read().unwrap())
                .unwrap();
        }
        let root = tmp.path().join("ws");
        let ws = Workspace::open(&root).unwrap();
        std::fs::create_dir_all(root.join("half")).unwrap();
        std::fs::write(root.join("half/placed.rs"), "from the interrupted fill").unwrap();
        let mut intent = Applied::default();
        intent.insert("half/placed.rs".parse().unwrap());
        intent.insert("half/never-placed.rs".parse().unwrap());
        let (applied, _) = ws
            .materialize(
                &cas,
                Plan {
                    manifest: &next.manifest,
                    previous: &intent,
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(!root.join("half").exists());
        assert_eq!(std::fs::read(root.join("kept.rs")).unwrap(), b"kept");
        assert_eq!(applied.paths().count(), 1);
        assert!(
            Workspace::open_existing(&tmp.path().join("absent"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn workspaces_follow_manifests_and_keep_untracked_build_output() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("lib/a.rs"), "a").unwrap();
        std::fs::write(src.join("gone.txt"), "bye").unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let upload = |snapshot: &crate::snapshot::Snapshot| {
            for blob in cas.missing(&snapshot.manifest.blobs()).unwrap() {
                cas.put(&blob, &snapshot.origins.get(&blob).unwrap().read().unwrap())
                    .unwrap();
            }
        };
        let first = from_directory(&src).unwrap();
        upload(&first);
        let ws = Workspace::open(&tmp.path().join("ws")).unwrap();
        let (applied, changes) = ws
            .materialize(
                &cas,
                Plan {
                    manifest: &first.manifest,
                    previous: &Applied::default(),
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            changes,
            Changes {
                written: 2,
                kept: 0,
                removed: 0
            }
        );
        std::fs::create_dir_all(ws.root().join("target")).unwrap();
        std::fs::write(ws.root().join("target/cache"), "warm").unwrap();

        std::fs::remove_file(src.join("gone.txt")).unwrap();
        std::fs::write(src.join("lib/a.rs"), "changed").unwrap();
        std::fs::write(src.join("new.txt"), "new").unwrap();
        let second = from_directory(&src).unwrap();
        upload(&second);
        let (_, second_changes) = ws
            .materialize(
                &cas,
                Plan {
                    manifest: &second.manifest,
                    previous: &applied,
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            second_changes,
            Changes {
                written: 2,
                kept: 0,
                removed: 1
            }
        );
        assert_eq!(
            std::fs::read_to_string(ws.root().join("lib/a.rs")).unwrap(),
            "changed"
        );
        assert_eq!(
            std::fs::read_to_string(ws.root().join("target/cache")).unwrap(),
            "warm"
        );
    }

    #[test]
    fn what_a_family_writes_it_reads_back_unchanged_whatever_system_runs_the_test() {
        for family in [crate::platform::FAMILY, crate::paths::Family::Windows] {
            let tmp = tempfile::tempdir().unwrap();
            let cas = Cas::open(tmp.path().join("cas")).unwrap();
            let stored = |text: &[u8], mode| {
                let blob = crate::domain::BlobId::of(text);
                cas.put(&blob, text).unwrap();
                Entry::File {
                    blob,
                    size: crate::domain::len_u64(text.len()),
                    mode,
                }
            };
            let manifest = Manifest {
                entries: std::collections::BTreeMap::from([
                    ("run.sh".parse().unwrap(), stored(b"echo", Mode::Executable)),
                    (
                        "notes.txt".parse().unwrap(),
                        stored(b"notes", Mode::Regular),
                    ),
                    (
                        "latest".parse().unwrap(),
                        Entry::Symlink {
                            target: "notes.txt".to_owned(),
                        },
                    ),
                ]),
            };
            let ws = Workspace::open_as(&tmp.path().join("ws"), family).unwrap();
            let fill = |previous: &Applied| {
                ws.materialize(
                    &cas,
                    Plan {
                        manifest: &manifest,
                        previous,
                    },
                    &AtomicBool::new(false),
                )
                .unwrap()
            };
            let (applied, _) = fill(&Applied::default());
            assert_eq!(ws.left(&manifest).unwrap(), Vec::new(), "{family:?}");
            let (_, again) = fill(&applied);
            assert_eq!((again.written, again.kept), (0, 3), "{family:?}");
        }
    }

    #[test]
    fn a_link_to_an_absolute_target_is_placed_and_then_recognised() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let manifest = Manifest {
            entries: std::collections::BTreeMap::from([(
                "hosts".parse().unwrap(),
                Entry::Symlink {
                    target: "/etc/hosts".to_owned(),
                },
            )]),
        };
        let ws = Workspace::open(&tmp.path().join("ws")).unwrap();
        let fill = |previous: &Applied| {
            ws.materialize(
                &cas,
                Plan {
                    manifest: &manifest,
                    previous,
                },
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        let (applied, first) = fill(&Applied::default());
        assert_eq!(first.written, 1);
        let (_, second) = fill(&applied);
        assert_eq!((second.written, second.kept), (0, 1));
        assert!(ws.left(&manifest).unwrap().is_empty());
    }

    #[test]
    fn symlinks_left_by_a_job_cannot_carry_writes_or_reads_outside() {
        if !crate::platform::LINKS {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "private").unwrap();
        let ws = Workspace::open(&tmp.path().join("ws")).unwrap();
        crate::platform::make_link(&outside.display().to_string(), &ws.root().join("escape"))
            .unwrap();
        ws.open_file(&"escape/secret".parse().unwrap()).unwrap_err();

        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let blob = crate::domain::BlobId::of(b"planted");
        cas.put(&blob, b"planted").unwrap();
        let manifest = Manifest {
            entries: std::collections::BTreeMap::from([(
                "escape/planted".parse().unwrap(),
                Entry::File {
                    blob,
                    size: 7,
                    mode: Mode::Regular,
                },
            )]),
        };
        ws.materialize(
            &cas,
            Plan {
                manifest: &manifest,
                previous: &Applied::default(),
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(outside.join("planted"))
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert!(
            std::fs::symlink_metadata(ws.root().join("escape"))
                .unwrap()
                .is_dir()
        );
    }
}
