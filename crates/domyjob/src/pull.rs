use crate::failure::io;
use std::collections::BTreeMap;
use std::io::{ErrorKind, Write as _};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{BlobId, RelPath};
use crate::lock::OsLock;
use crate::snapshot::{Entry, Left, Manifest};
use crate::tree::{Blockers, Contents, Placed, Removed, Rooted, TreeError};

const KEPT_PULLS: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("{} changed here since the job was sent: {}", .0.len(), list(.0))]
    Diverged(Vec<RelPath>),
    #[error("{} changed here since they were pulled: {}", .0.len(), list(.0))]
    Edited(Vec<RelPath>),
    #[error("{0} is gone, so there is nowhere to put the changes")]
    Gone(PathBuf),
    #[error("nothing from {0} was pulled on this machine")]
    NeverPulled(String),
    #[error("the original of {0} was not kept when it was pulled")]
    NotKept(RelPath),
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
    #[error(transparent)]
    Malformed(#[from] Malformed),
    #[error(transparent)]
    Tree(#[from] TreeError),
}

#[derive(Debug, thiserror::Error)]
pub enum Malformed {
    #[error("the list of what was sent is not the one this machine sent")]
    OtherManifest,
    #[error("the list of what was sent is not valid: {0}")]
    Manifest(serde_json::Error),
    #[error("{0} is listed out of order or twice")]
    Unordered(RelPath),
    #[error("{0} arrived without its content")]
    Missing(RelPath),
    #[error("{0} does not match its digest")]
    Damaged(RelPath),
    #[error("{0} arrived but is not a changed file")]
    Extra(RelPath),
}

fn list(paths: &[RelPath]) -> String {
    paths
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentManifest(Manifest);

impl SentManifest {
    pub fn verified(raw: &[u8], recorded: &BlobId) -> Result<Self, Malformed> {
        if BlobId::of(raw) != *recorded {
            return Err(Malformed::OtherManifest);
        }
        crate::ingress::json(raw)
            .map(Self)
            .map_err(Malformed::Manifest)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Blobs(BTreeMap<BlobId, Vec<u8>>);

impl Blobs {
    fn insert(&mut self, bytes: Vec<u8>) -> BlobId {
        let id = BlobId::of(&bytes);
        self.0.insert(id.clone(), bytes);
        id
    }

    fn get(&self, id: &BlobId) -> Option<&[u8]> {
        self.0.get(id).map(Vec::as_slice)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Modified,
    Removed,
}

impl Kind {
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Removed => 'D',
        }
    }

    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub path: RelPath,
    pub before: Option<Entry>,
    pub after: Option<Entry>,
}

impl Step {
    #[must_use]
    pub const fn kind(&self) -> Kind {
        match (&self.before, &self.after) {
            (None, _) => Kind::Added,
            (Some(_), None) => Kind::Removed,
            (Some(_), Some(_)) => Kind::Modified,
        }
    }

    fn reversed(&self) -> Self {
        Self {
            path: self.path.clone(),
            before: self.after.clone(),
            after: self.before.clone(),
        }
    }
}

pub trait Direction {
    fn stuck(paths: Vec<RelPath>) -> PullError;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forward;

impl Direction for Forward {
    fn stuck(paths: Vec<RelPath>) -> PullError {
        PullError::Diverged(paths)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Back;

impl Direction for Back {
    fn stuck(paths: Vec<RelPath>) -> PullError {
        PullError::Edited(paths)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan<Towards> {
    steps: Vec<Step>,
    contents: Blobs,
    direction: PhantomData<Towards>,
}

impl Plan<Forward> {
    pub fn new(
        sent: &SentManifest,
        left: Vec<Left>,
        mut contents: BTreeMap<RelPath, Vec<u8>>,
    ) -> Result<Self, Malformed> {
        let mut steps = Vec::new();
        let mut blobs = Blobs::default();
        let mut previous: Option<RelPath> = None;
        for Left { path, now } in left {
            if previous.as_ref().is_some_and(|previous| *previous >= path) {
                return Err(Malformed::Unordered(path));
            }
            previous = Some(path.clone());
            if let Some(Entry::File { blob, size, .. }) = &now {
                let bytes = contents
                    .remove(&path)
                    .ok_or_else(|| Malformed::Missing(path.clone()))?;
                if crate::domain::len_u64(bytes.len()) != *size || blobs.insert(bytes) != *blob {
                    return Err(Malformed::Damaged(path));
                }
            }
            let before = sent.0.entries.get(&path).cloned();
            if before != now {
                steps.push(Step {
                    path,
                    before,
                    after: now,
                });
            }
        }
        match contents.into_keys().next() {
            Some(extra) => Err(Malformed::Extra(extra)),
            None => Ok(Self {
                steps,
                contents: blobs,
                direction: PhantomData,
            }),
        }
    }
}

impl<Towards: Direction> Plan<Towards> {
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn check(self, tree: &Tree) -> Result<Checked<Towards>, PullError> {
        let removed: Removed = self
            .steps
            .iter()
            .filter(|step| step.after.is_none())
            .map(|step| step.path.clone())
            .collect();
        let mut stuck = Vec::new();
        let mut pending = Vec::new();
        let mut already = 0usize;
        for step in self.steps {
            if tree.holds(&step.path, step.after.as_ref(), &Removed::new())? {
                already = already.saturating_add(1);
            } else if tree.holds(&step.path, step.before.as_ref(), &removed)?
                && (step.after.is_none() || tree.placeable(&step.path, &removed)?)
            {
                pending.push(step);
            } else {
                stuck.push(step.path);
            }
        }
        if !stuck.is_empty() {
            return Err(Towards::stuck(stuck));
        }
        Ok(Checked {
            pending: Self {
                steps: pending,
                contents: self.contents,
                direction: PhantomData,
            },
            already,
        })
    }
}

#[derive(Debug)]
pub struct Checked<Towards> {
    pending: Plan<Towards>,
    already: usize,
}

#[derive(Debug)]
pub struct Kept(Checked<Forward>);

impl Checked<Forward> {
    pub fn keep(self, tree: &Tree, journal: &Journal) -> Result<Kept, PullError> {
        journal.record(tree, &self.pending.steps)?;
        for step in &self.pending.steps {
            if let Some(entry @ Entry::File { blob, .. }) = &step.before
                && tree.holds(&step.path, Some(entry), &Removed::new())?
            {
                journal.keep(blob, &tree.read(&step.path)?, &step.path)?;
            }
        }
        Ok(Kept(self))
    }
}

impl Kept {
    pub fn apply(self, tree: &Tree, journal: &Journal) -> Result<Applied, PullError> {
        apply(&self.0, tree, journal)
    }
}

impl Checked<Back> {
    pub fn apply(self, tree: &Tree, journal: &Journal) -> Result<Applied, PullError> {
        apply(&self, tree, journal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    pub changed: usize,
    pub already: usize,
}

fn depth(path: &RelPath) -> usize {
    path.parts().count()
}

fn ordered<Towards>(plan: &Plan<Towards>) -> Vec<&Step> {
    let (mut removals, mut placements): (Vec<&Step>, Vec<&Step>) =
        plan.steps.iter().partition(|step| step.after.is_none());
    removals.sort_by(|a, b| {
        depth(&b.path)
            .cmp(&depth(&a.path))
            .then(a.path.cmp(&b.path))
    });
    placements.sort_by(|a, b| {
        depth(&a.path)
            .cmp(&depth(&b.path))
            .then(a.path.cmp(&b.path))
    });
    removals.into_iter().chain(placements).collect()
}

fn apply<Towards: Direction>(
    checked: &Checked<Towards>,
    tree: &Tree,
    journal: &Journal,
) -> Result<Applied, PullError> {
    journal.sweep(tree)?;
    let plan = &checked.pending;
    let mut applied = Applied {
        changed: 0,
        already: checked.already,
    };
    for step in ordered(plan) {
        let none = Removed::new();
        if tree.holds(&step.path, step.after.as_ref(), &none)? {
            applied.already = applied.already.saturating_add(1);
            continue;
        }
        let Some(seen) = tree.see(&step.path, step.before.as_ref())? else {
            return Err(Towards::stuck(vec![step.path.clone()]));
        };
        let to = match &step.after {
            None => To::Absent,
            Some(Entry::File { blob, mode, .. }) => To::Present(Placed::File(
                plan.contents
                    .get(blob)
                    .ok_or_else(|| PullError::NotKept(step.path.clone()))?,
                *mode,
            )),
            Some(Entry::Symlink { target }) => To::Present(Placed::Link(target)),
        };
        tree.swap(seen, to, journal)?;
        applied.changed = applied.changed.saturating_add(1);
    }
    journal.sweep(tree)?;
    Ok(applied)
}

#[derive(Debug)]
pub struct Tree(Rooted);

#[derive(Debug)]
pub struct Seen<'tree> {
    path: RelPath,
    tree: PhantomData<&'tree Tree>,
}

#[derive(Debug, Clone, Copy)]
enum To<'a> {
    Absent,
    Present(Placed<'a>),
}

fn diverged(error: TreeError) -> PullError {
    match error {
        TreeError::Blocked(path) | TreeError::Occupied(path) => PullError::Diverged(vec![path]),
        other @ (TreeError::Io { .. } | TreeError::Unportable(_)) => PullError::Tree(other),
    }
}

impl Tree {
    pub fn open(root: &Path) -> Result<Self, PullError> {
        Rooted::open(root)?
            .map(Self)
            .ok_or_else(|| PullError::Gone(root.to_path_buf()))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.0.root()
    }

    fn holds(
        &self,
        rel: &RelPath,
        expected: Option<&Entry>,
        removed: &Removed,
    ) -> Result<bool, PullError> {
        Ok(self.0.holds(rel, expected, removed)?)
    }

    fn placeable(&self, rel: &RelPath, removed: &Removed) -> Result<bool, PullError> {
        Ok(self.0.placeable(rel, removed)?)
    }

    fn see(&self, rel: &RelPath, expected: Option<&Entry>) -> Result<Option<Seen<'_>>, PullError> {
        Ok(self.holds(rel, expected, &Removed::new())?.then(|| Seen {
            path: rel.clone(),
            tree: PhantomData,
        }))
    }

    fn read(&self, rel: &RelPath) -> Result<Vec<u8>, PullError> {
        Ok(self.0.read(rel)?)
    }

    fn swap(&self, seen: Seen<'_>, to: To<'_>, journal: &Journal) -> Result<(), PullError> {
        let rel = seen.path;
        let shown = self.0.shown(&rel);
        crate::faults::at("pull::swap", &shown).map_err(io("changing", &shown))?;
        match to {
            To::Absent => {
                self.0
                    .clear(&rel, Contents::EmptyDirectoriesOnly)
                    .map_err(diverged)?;
                self.0.prune(&rel);
                Ok(())
            }
            To::Present(placed) => {
                self.0
                    .make_parents(&rel, Blockers::Refuse)
                    .map_err(diverged)?;
                self.0
                    .clear_directory(&rel, Contents::EmptyDirectoriesOnly)
                    .map_err(diverged)?;
                let staging = journal.staging(&rel)?;
                self.0.create(&staging, placed)?;
                Ok(self.0.rename(&staging, &rel)?)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    root: PathBuf,
    steps: Vec<Step>,
}

impl crate::ingress::Ingress for Record {}

#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    _lock: OsLock,
}

impl Journal {
    pub fn open(pulls: &Path, name: &str) -> Result<Self, PullError> {
        crate::state_file::private_dir(pulls)?;
        let dir = match Self::existing(pulls, name)? {
            Some(dir) => dir,
            None => {
                let journals = Self::listed(pulls)?;
                let next = journals
                    .last()
                    .and_then(|(sequence, _)| sequence.checked_add(1))
                    .unwrap_or(0);
                let excess = journals.len().saturating_add(1).saturating_sub(KEPT_PULLS);
                for (_, old) in journals.iter().take(excess) {
                    if let Some(lock) = OsLock::try_exclusive(&old.join("lock"))? {
                        crate::state_file::remove_dir_all(old)?;
                        drop(lock);
                    }
                }
                pulls.join(format!("{next:010}-{name}"))
            }
        };
        crate::state_file::private_dir(&dir)?;
        let lock = OsLock::exclusive(&dir.join("lock"))?;
        Ok(Self { dir, _lock: lock })
    }

    pub fn find(pulls: &Path, name: &str, shown: &str) -> Result<Self, PullError> {
        match Self::existing(pulls, name)? {
            Some(dir) => {
                let lock = OsLock::exclusive(&dir.join("lock"))?;
                Ok(Self { dir, _lock: lock })
            }
            None => Err(PullError::NeverPulled(shown.to_owned())),
        }
    }

    fn listed(pulls: &Path) -> Result<Vec<(u64, PathBuf)>, PullError> {
        let listing = match std::fs::read_dir(pulls) {
            Ok(listing) => listing,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io("listing", pulls)(error).into()),
        };
        let mut found = Vec::new();
        for item in listing {
            let item = item.map_err(io("listing", pulls))?;
            let name = item.file_name();
            let sequence = name
                .to_str()
                .and_then(|name| name.split_once('-'))
                .map(|(sequence, _)| sequence.parse::<u64>());
            if let Some(Ok(sequence)) = sequence {
                found.push((sequence, item.path()));
            }
        }
        found.sort();
        Ok(found)
    }

    fn existing(pulls: &Path, name: &str) -> Result<Option<PathBuf>, PullError> {
        Ok(Self::listed(pulls)?.into_iter().find_map(|(_, dir)| {
            dir.file_name()
                .and_then(|file| file.to_str())
                .and_then(|file| file.split_once('-'))
                .is_some_and(|(_, rest)| rest == name)
                .then_some(dir)
        }))
    }

    fn record(&self, tree: &Tree, pending: &[Step]) -> Result<(), PullError> {
        let path = self.dir.join("plan.json");
        let mut steps: BTreeMap<RelPath, Step> =
            match crate::state_file::read_json::<Record>(&path)? {
                Some(earlier) => earlier
                    .steps
                    .into_iter()
                    .map(|step| (step.path.clone(), step))
                    .collect(),
                None => BTreeMap::new(),
            };
        for step in pending {
            steps
                .entry(step.path.clone())
                .or_insert_with(|| step.clone());
        }
        Ok(crate::state_file::write_json(
            &path,
            &Record {
                root: tree.root().to_path_buf(),
                steps: steps.into_values().collect(),
            },
        )?)
    }

    fn keep(&self, blob: &BlobId, bytes: &[u8], rel: &RelPath) -> Result<(), PullError> {
        if BlobId::of(bytes) != *blob {
            return Err(PullError::Diverged(vec![rel.clone()]));
        }
        Ok(crate::state_file::write_bytes(
            &self.dir.join("kept").join(blob.as_str()),
            bytes,
        )?)
    }

    fn staging(&self, rel: &RelPath) -> Result<RelPath, PullError> {
        let nonce = crate::domain::Nonce::generate().map_err(|error| {
            io("naming a file beside", &self.dir)(std::io::Error::other(error.to_string()))
        })?;
        let name = rel.parts().next_back().unwrap_or_default();
        let unique = nonce.as_str().get(..12).unwrap_or_default();
        let beside = format!(".{name}.{unique}.domyjob-pull");
        let staging: RelPath = match crate::tree::ancestors(rel).last() {
            Some(parent) => format!("{parent}/{beside}"),
            None => beside,
        }
        .parse()
        .map_err(|error: crate::domain::Invalid| {
            io("naming a file beside", &self.dir)(std::io::Error::other(error.to_string()))
        })?;
        let mut line = staging.to_string().into_bytes();
        line.push(b'\n');
        let path = self.dir.join("staging");
        crate::state_file::open_append(&path)?
            .write_all(&line)
            .map_err(io("writing", &path))?;
        Ok(staging)
    }

    fn sweep(&self, tree: &Tree) -> Result<(), PullError> {
        let path = self.dir.join("staging");
        let Some(bytes) = crate::state_file::read_bytes(&path)? else {
            return Ok(());
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Ok(staging) = line.parse::<RelPath>() {
                tree.0.remove_file(&staging)?;
            }
        }
        Ok(crate::state_file::remove_file(&path)?)
    }

    pub fn undo(&self) -> Result<(Tree, Plan<Back>), PullError> {
        let record: Record = crate::state_file::read_json(&self.dir.join("plan.json"))?
            .ok_or_else(|| PullError::NeverPulled(self.dir.display().to_string()))?;
        let tree = Tree::open(&record.root)?;
        let mut contents = Blobs::default();
        let mut steps = Vec::with_capacity(record.steps.len());
        for step in &record.steps {
            let back = step.reversed();
            if let Some(Entry::File { blob, .. }) = &back.after {
                let kept =
                    crate::state_file::read_bytes(&self.dir.join("kept").join(blob.as_str()))?
                        .ok_or_else(|| PullError::NotKept(step.path.clone()))?;
                if contents.insert(kept) != *blob {
                    return Err(PullError::NotKept(step.path.clone()));
                }
            }
            steps.push(back);
        }
        Ok((
            tree,
            Plan {
                steps,
                contents,
                direction: PhantomData,
            },
        ))
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests lay out and read back the local project directly"
)]
mod tests {
    use super::*;
    use crate::snapshot::Mode;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    const PATHS: &[&str] = &["a", "b", "d", "d/x", "d/y", "e/f/g", "l", "l/x"];
    const CONTENTS: &[&[u8]] = &[b"", b"one", b"two", b"three"];
    const TARGETS: &[&str] = &["../outside", "a", "/nowhere/at/all"];

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Node {
        File(usize, bool),
        Link(usize),
    }

    type Layout = BTreeMap<RelPath, Node>;

    fn node() -> impl Strategy<Value = Node> {
        let file = (0..CONTENTS.len(), any::<bool>()).prop_map(|(content, executable)| {
            Node::File(content, crate::platform::MODES && executable)
        });
        prop_oneof![3 => file, 1 => (0..TARGETS.len()).prop_map(Node::Link)]
    }

    fn layout() -> impl Strategy<Value = Layout> {
        proptest::collection::btree_map(0..PATHS.len(), node(), 0..=5).prop_map(|picked| {
            let all: Layout = picked
                .into_iter()
                .map(|(index, node)| (PATHS.get(index).unwrap().parse().unwrap(), node))
                .collect();
            all.iter()
                .filter(|(path, _)| {
                    crate::tree::ancestors(path)
                        .iter()
                        .all(|ancestor| !all.contains_key(ancestor))
                })
                .map(|(path, node)| (path.clone(), node.clone()))
                .collect()
        })
    }

    fn entry(node: &Node) -> Entry {
        match node {
            Node::File(content, executable) => {
                let bytes = CONTENTS.get(*content).unwrap();
                Entry::File {
                    blob: BlobId::of(bytes),
                    size: crate::domain::len_u64(bytes.len()),
                    mode: if *executable {
                        Mode::Executable
                    } else {
                        Mode::Regular
                    },
                }
            }
            Node::Link(target) => Entry::Symlink {
                target: (*TARGETS.get(*target).unwrap()).to_owned(),
            },
        }
    }

    fn lay_out(root: &Path, layout: &Layout) {
        std::fs::create_dir_all(root).unwrap();
        for (path, node) in layout {
            let at = root.join(path.to_local());
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            match node {
                Node::File(content, executable) => {
                    std::fs::write(&at, CONTENTS.get(*content).unwrap()).unwrap();
                    let mode = if *executable {
                        Mode::Executable
                    } else {
                        Mode::Regular
                    };
                    crate::platform::set_mode(&at, mode).unwrap();
                }
                Node::Link(target) if crate::platform::LINKS => {
                    crate::platform::make_link(TARGETS.get(*target).unwrap(), &at).unwrap();
                }
                Node::Link(target) => std::fs::write(&at, TARGETS.get(*target).unwrap()).unwrap(),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Found {
        File(Vec<u8>, bool),
        Link(String),
    }

    fn read_back(root: &Path) -> BTreeMap<String, Found> {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for item in std::fs::read_dir(&dir).unwrap() {
                let path = item.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if meta.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    found.insert(name, Found::Link(target.to_string_lossy().into_owned()));
                } else if meta.is_dir() {
                    pending.push(path);
                } else {
                    found.insert(
                        name,
                        Found::File(std::fs::read(&path).unwrap(), executable(&meta)),
                    );
                }
            }
        }
        found
    }

    fn executable(meta: &std::fs::Metadata) -> bool {
        crate::platform::Moded::mode(meta) == Mode::Executable
    }

    fn expected(layout: &Layout) -> BTreeMap<String, Found> {
        layout
            .iter()
            .map(|(path, node)| {
                let found = match node {
                    Node::File(content, executable) => {
                        Found::File(CONTENTS.get(*content).unwrap().to_vec(), *executable)
                    }
                    Node::Link(target) if crate::platform::LINKS => {
                        Found::Link((*TARGETS.get(*target).unwrap()).to_owned())
                    }
                    Node::Link(target) => {
                        Found::File(TARGETS.get(*target).unwrap().as_bytes().to_vec(), false)
                    }
                };
                (path.to_string(), found)
            })
            .collect()
    }

    fn manifest(layout: &Layout) -> Manifest {
        Manifest {
            entries: layout
                .iter()
                .map(|(path, node)| (path.clone(), entry(node)))
                .collect(),
        }
    }

    fn plan(sent: &Layout, after: &Layout) -> Plan<Forward> {
        let (id, raw) = manifest(sent).encode().unwrap();
        let sent_manifest = SentManifest::verified(&raw, &id).unwrap();
        let paths: BTreeSet<&RelPath> = sent.keys().chain(after.keys()).collect();
        let mut contents = BTreeMap::new();
        let left = paths
            .into_iter()
            .filter(|path| sent.get(*path) != after.get(*path))
            .map(|path| {
                if let Some(Node::File(content, _)) = after.get(path) {
                    contents.insert(path.clone(), CONTENTS.get(*content).unwrap().to_vec());
                }
                Left {
                    path: path.clone(),
                    now: after.get(path).map(entry),
                }
            })
            .collect();
        Plan::new(&sent_manifest, left, contents).unwrap()
    }

    fn undo_all(pulls: &Path, name: &str) {
        let journal = Journal::find(pulls, name, name).unwrap();
        let (tree, back) = journal.undo().unwrap();
        back.check(&tree).unwrap().apply(&tree, &journal).unwrap();
    }

    fn kept_pull(root: &Path, pulls: &Path) -> (Tree, Journal, Kept, BTreeMap<String, Found>) {
        let file = |content: usize| Node::File(content, false);
        let sent: Layout = [
            ("a".parse().unwrap(), file(1)),
            ("b".parse().unwrap(), file(2)),
            ("d/x".parse().unwrap(), file(3)),
        ]
        .into();
        let after: Layout = [
            ("a".parse().unwrap(), file(2)),
            ("d/y".parse().unwrap(), file(1)),
        ]
        .into();
        lay_out(root, &sent);
        let untouched = read_back(root);
        let tree = Tree::open(root).unwrap();
        let journal = Journal::open(pulls, "m-job").unwrap();
        let kept = plan(&sent, &after)
            .check(&tree)
            .unwrap()
            .keep(&tree, &journal)
            .unwrap();
        (tree, journal, kept, untouched)
    }

    #[test]
    fn a_crash_at_every_workspace_step_can_be_undone_from_the_journal() {
        let steps = {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("project");
            let pulls = tmp.path().join("pulls");
            let (tree, journal, kept, _) = kept_pull(&root, &pulls);
            let crashing = crate::faults::crash_after(tmp.path(), None);
            kept.apply(&tree, &journal).unwrap();
            crashing.steps()
        };
        assert!(steps > 0);
        for step in 0..steps {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("project");
            let pulls = tmp.path().join("pulls");
            let (tree, journal, kept, untouched) = kept_pull(&root, &pulls);
            {
                let _crashing = crate::faults::crash_after(tmp.path(), Some(step));
                match kept.apply(&tree, &journal) {
                    Ok(_) | Err(_) => {}
                }
            }
            drop(journal);
            undo_all(&pulls, "m-job");
            assert_eq!(read_back(&root), untouched, "after step {step}");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(192))]

        #[test]
        fn a_pull_changes_only_what_the_job_changed_and_can_always_be_put_back(
            sent in layout(),
            after in layout(),
            edited in proptest::option::weighted(0.25, layout()),
            fault in proptest::option::weighted(0.5, 0usize..6),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("project");
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret"), b"private").unwrap();
            let pulls = tmp.path().join("state").join("pulls");
            let before = edited.unwrap_or_else(|| sent.clone());
            lay_out(&root, &before);
            let untouched = read_back(&root);
            prop_assert_eq!(&untouched, &expected(&before));

            let plan = plan(&sent, &after);
            let steps: BTreeSet<String> =
                plan.steps().iter().map(|step| step.path.to_string()).collect();
            let tree = Tree::open(&root).unwrap();
            let checked = match plan.check(&tree) {
                Ok(checked) => checked,
                Err(PullError::Diverged(_)) => {
                    prop_assert_eq!(read_back(&root), untouched);
                    prop_assert_eq!(read_back(&outside).len(), 1);
                    return Ok(());
                }
                Err(other) => panic!("{other}"),
            };
            let journal = Journal::open(&pulls, "m-job").unwrap();
            let kept = checked.keep(&tree, &journal).unwrap();
            let injected = fault.map(|passes| crate::faults::inject_after("pull::swap", passes, ""));
            let pulled = kept.apply(&tree, &journal);
            drop(injected);
            drop(journal);
            let now = read_back(&root);
            let target = {
                let mut target: BTreeMap<String, Found> = untouched
                    .iter()
                    .filter(|(path, _)| !steps.contains(*path))
                    .map(|(path, found)| (path.clone(), found.clone()))
                    .collect();
                target.extend(
                    expected(&after)
                        .into_iter()
                        .filter(|(path, _)| steps.contains(path)),
                );
                target
            };
            match pulled {
                Ok(_) => prop_assert_eq!(&now, &target),
                Err(_) => {
                    for (path, found) in &now {
                        let unchanged = untouched.get(path) == Some(found);
                        let changed = target.get(path) == Some(found);
                        let staging = path.ends_with(".domyjob-pull");
                        prop_assert!(unchanged || changed || staging, "{path} is {found:?}");
                    }
                    for path in untouched.keys().filter(|path| !steps.contains(*path)) {
                        prop_assert_eq!(now.get(path), untouched.get(path));
                    }
                }
            }
            prop_assert_eq!(read_back(&outside).len(), 1);

            undo_all(&pulls, "m-job");
            prop_assert_eq!(read_back(&root), untouched);
            prop_assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"private");
        }
    }

    #[test]
    fn nothing_is_written_through_a_link_or_where_the_user_edited() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let file = |content: usize| Node::File(content, false);
        let sent: Layout = [("a".parse().unwrap(), file(1))].into();
        let mut local = sent.clone();
        local.insert("l".parse().unwrap(), Node::Link(0));
        lay_out(&root, &local);
        let tree = Tree::open(&root).unwrap();
        let through_link: Layout = [
            ("a".parse().unwrap(), file(1)),
            ("l/x".parse().unwrap(), file(2)),
        ]
        .into();
        let refused = plan(&sent, &through_link).check(&tree);
        assert!(
            matches!(&refused, Err(PullError::Diverged(paths)) if paths.len() == 1),
            "{refused:?}"
        );
        std::fs::write(root.join("a"), b"mine").unwrap();
        let edited: Layout = [("a".parse().unwrap(), file(2))].into();
        assert!(matches!(
            plan(&sent, &edited).check(&tree),
            Err(PullError::Diverged(_))
        ));
        assert_eq!(std::fs::read(root.join("a")).unwrap(), b"mine");
    }

    #[test]
    fn a_plan_lists_only_paths_whose_state_differs_from_what_was_sent() {
        let sent: Layout = [
            ("a".parse().unwrap(), Node::File(1, false)),
            ("b".parse().unwrap(), Node::File(2, false)),
        ]
        .into();
        let after: Layout = [
            ("a".parse().unwrap(), Node::File(1, crate::platform::MODES)),
            ("d/x".parse().unwrap(), Node::File(3, false)),
        ]
        .into();
        let kinds: String = plan(&sent, &after)
            .steps()
            .iter()
            .map(|step| step.kind().letter())
            .collect();
        assert_eq!(kinds, if crate::platform::MODES { "MDA" } else { "DA" });
        assert!(plan(&sent, &sent).steps().is_empty());
    }
}
