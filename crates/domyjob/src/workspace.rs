use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cap_std::fs::{Dir, OpenOptions};

use crate::cas::{Applied, Cas, CasError};
use crate::domain::RelPath;
use crate::snapshot::{Entry, Left, Manifest, Mode};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("{action} {path} inside the workspace: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error("the workspace was left half-updated because the job was stopped")]
    Stopped,
    #[error("{0} is a directory; get copies one file, so name a file inside it")]
    NotAFile(PathBuf),
    #[error(transparent)]
    Snapshot(#[from] crate::snapshot::SnapshotError),
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> WorkspaceError + use<> {
    let path = path.to_path_buf();
    move |source| WorkspaceError::Io {
        action,
        path,
        source,
    }
}

fn relative(rel: &RelPath) -> PathBuf {
    rel.parts().collect()
}

#[derive(Debug)]
pub struct Workspace {
    dir: Dir,
    root: PathBuf,
}

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
        crate::state_file::private_dir(root)?;
        let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .map_err(io("opening", root))?;
        Ok(Self {
            dir,
            root: root.to_path_buf(),
        })
    }

    pub fn open_existing(root: &Path) -> Result<Option<Self>, WorkspaceError> {
        match Dir::open_ambient_dir(root, cap_std::ambient_authority()) {
            Ok(dir) => Ok(Some(Self {
                dir,
                root: root.to_path_buf(),
            })),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("opening", root)(e)),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn content_is(
        &self,
        path: &Path,
        blob: &crate::domain::BlobId,
    ) -> Result<bool, WorkspaceError> {
        Ok(&self.digest(path)? == blob)
    }

    fn digest(&self, path: &Path) -> Result<crate::domain::BlobId, WorkspaceError> {
        let mut file = self.dir.open(path).map_err(io("opening", path))?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(io("reading", path))?;
            match buffer.get(..read) {
                Some([]) | None => break,
                Some(chunk) => {
                    hasher.update(chunk);
                }
            }
        }
        Ok(crate::domain::BlobId::from_hash(&hasher.finalize()))
    }

    fn holds(&self, path: &Path, entry: &Entry) -> Result<bool, WorkspaceError> {
        if !self.under_directories(path)? {
            return Ok(false);
        }
        let meta = match self.dir.symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {
                return Ok(false);
            }
            Err(e) => return Err(io("checking", path)(e)),
        };
        match entry {
            Entry::File { blob, size, mode } => {
                if !meta.is_file()
                    || meta.len() != *size
                    || crate::platform::Moded::mode(&meta) != *mode
                {
                    return Ok(false);
                }
                self.content_is(path, blob)
            }
            Entry::Symlink { target } => self.holds_link(path, &meta, target),
        }
    }

    fn holds_link(
        &self,
        path: &Path,
        meta: &cap_std::fs::Metadata,
        target: &str,
    ) -> Result<bool, WorkspaceError> {
        if !crate::platform::LINKS {
            return if meta.is_file() && meta.len() == crate::domain::len_u64(target.len()) {
                self.content_is(path, &crate::domain::BlobId::of(target.as_bytes()))
            } else {
                Ok(false)
            };
        }
        if !meta.is_symlink() {
            return Ok(false);
        }
        let found = self
            .dir
            .read_link_contents(path)
            .map_err(io("reading", path))?;
        Ok(found.as_os_str() == target)
    }

    fn clear(&self, path: &Path) -> Result<(), WorkspaceError> {
        match self.dir.symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {
                self.dir.remove_dir_all(path).map_err(io("removing", path))
            }
            Ok(_) => self.dir.remove_file(path).map_err(io("removing", path)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io("checking", path)(e)),
        }
    }

    fn prepare_parent(&self, path: &Path) -> Result<(), WorkspaceError> {
        let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
            return Ok(());
        };
        let mut walked = PathBuf::new();
        for component in parent.components() {
            walked.push(component);
            match self.dir.symlink_metadata(&walked) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    self.dir
                        .remove_file(&walked)
                        .map_err(io("replacing", &walked))?;
                    self.dir
                        .create_dir(&walked)
                        .map_err(io("creating", &walked))?;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    self.dir
                        .create_dir(&walked)
                        .map_err(io("creating", &walked))?;
                }
                Err(e) => return Err(io("checking", &walked)(e)),
            }
        }
        Ok(())
    }

    fn write_file(&self, path: &Path, bytes: &[u8], mode: Mode) -> Result<(), WorkspaceError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        crate::platform::create_as(&mut options, mode);
        let mut file = self
            .dir
            .open_with(path, &options)
            .map_err(io("creating", path))?;
        file.write_all(bytes).map_err(io("writing", path))
    }

    fn place(&self, cas: &Cas, path: &Path, entry: &Entry) -> Result<(), WorkspaceError> {
        self.prepare_parent(path)?;
        self.clear(path)?;
        match entry {
            Entry::File { blob, mode, .. } => self.write_file(path, &cas.get(blob)?, *mode),
            Entry::Symlink { target } => self.place_symlink(target, path),
        }
    }

    fn place_symlink(&self, target: &str, path: &Path) -> Result<(), WorkspaceError> {
        if crate::platform::LINKS {
            crate::platform::link(&self.dir, target, path).map_err(io("linking", path))
        } else {
            self.write_file(path, target.as_bytes(), Mode::Regular)
        }
    }

    fn remove_emptied(&self, path: &Path) {
        for ancestor in path
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
        {
            match self.dir.remove_dir(ancestor) {
                Ok(()) => {}
                Err(_not_empty_or_gone) => break,
            }
        }
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
            let path = relative(rel);
            self.clear(&path)?;
            self.remove_emptied(&path);
            changes.removed = changes.removed.saturating_add(1);
        }
        let mut next = Applied::default();
        for (rel, entry) in &manifest.entries {
            if stop.load(Ordering::SeqCst) {
                return Err(WorkspaceError::Stopped);
            }
            let path = relative(rel);
            if self.holds(&path, entry)? {
                changes.kept = changes.kept.saturating_add(1);
            } else {
                self.place(cas, &path, entry)?;
                changes.written = changes.written.saturating_add(1);
            }
            next.insert(rel.clone());
        }
        Ok((next, changes))
    }

    pub fn left(&self, sent: &Manifest) -> Result<Vec<Left>, WorkspaceError> {
        let mut left = Vec::new();
        for (rel, entry) in &sent.entries {
            let path = relative(rel);
            if !self.holds(&path, entry)? {
                left.push(Left {
                    path: rel.clone(),
                    now: self.entry(&path)?,
                });
            }
        }
        for rel in crate::snapshot::inside_paths(&self.root)? {
            if !sent.entries.contains_key(&rel)
                && let Some(now) = self.entry(&relative(&rel))?
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

    fn under_directories(&self, path: &Path) -> Result<bool, WorkspaceError> {
        let mut walked = PathBuf::new();
        for component in path.parent().into_iter().flat_map(Path::components) {
            walked.push(component);
            match self.dir.symlink_metadata(&walked) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(false),
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(io("checking", &walked)(e)),
            }
        }
        Ok(true)
    }

    fn entry(&self, path: &Path) -> Result<Option<Entry>, WorkspaceError> {
        if !self.under_directories(path)? {
            return Ok(None);
        }
        let meta = match self.dir.symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io("checking", path)(e)),
        };
        if meta.is_symlink() {
            let target = self
                .dir
                .read_link_contents(path)
                .map_err(io("reading", path))?;
            let target = target
                .to_str()
                .ok_or_else(|| crate::snapshot::SnapshotError::Unportable(self.root.join(path)))?;
            return Ok(Some(Entry::Symlink {
                target: target.to_owned(),
            }));
        }
        if !meta.is_file() {
            return Ok(None);
        }
        Ok(Some(Entry::File {
            blob: self.digest(path)?,
            size: meta.len(),
            mode: crate::platform::Moded::mode(&meta),
        }))
    }

    pub fn open_file(&self, rel: &RelPath) -> Result<std::fs::File, WorkspaceError> {
        let path = relative(rel);
        let meta = self.dir.metadata(&path).map_err(io("opening", &path))?;
        if meta.is_dir() {
            return Err(WorkspaceError::NotAFile(path));
        }
        let file = self.dir.open(&path).map_err(io("opening", &path))?;
        Ok(file.into_std())
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build their fixtures directly on disk"
)]
mod tests {
    use super::*;
    use crate::snapshot::from_directory;

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
