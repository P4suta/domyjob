use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::domain::ChainHash;
use crate::lock::OsLock;
use crate::paths::Dirs;
use crate::state_file::{self, StateError};

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const ACTIVE_LIMIT: u64 = 8 << 20;

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error(
        "the audit log {path} is broken at entry {line}: {why}; it has been altered or truncated"
    )]
    Broken {
        path: PathBuf,
        line: u64,
        why: &'static str,
    },
    #[error("encoding an audit entry: {0}")]
    Encode(serde_json::Error),
    #[error(transparent)]
    Invalid(crate::domain::Invalid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allowed,
    Denied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, from = "StoredEntry")]
pub struct Entry {
    pub epoch: u64,
    pub seq: u64,
    pub at: Timestamp,
    pub prev: String,
    pub principal: String,
    pub action: String,
    pub subject: Option<String>,
    pub verdict: Verdict,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEntry {
    epoch: Option<u64>,
    seq: u64,
    at: Timestamp,
    prev: String,
    principal: String,
    action: String,
    subject: Option<String>,
    verdict: Verdict,
}

impl From<StoredEntry> for Entry {
    fn from(stored: StoredEntry) -> Self {
        Self {
            epoch: stored.epoch.unwrap_or(0),
            seq: stored.seq,
            at: stored.at,
            prev: stored.prev,
            principal: stored.principal,
            action: stored.action,
            subject: stored.subject,
            verdict: stored.verdict,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, from = "StoredHead")]
#[schemars(!from)]
pub struct Head {
    pub epoch: u64,
    pub seq: u64,
    pub hash: ChainHash,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHead {
    epoch: Option<u64>,
    seq: u64,
    hash: ChainHash,
}

impl From<StoredHead> for Head {
    fn from(stored: StoredHead) -> Self {
        Self {
            epoch: stored.epoch.unwrap_or(0),
            seq: stored.seq,
            hash: stored.hash,
        }
    }
}

impl Head {
    fn genesis() -> Result<Self, AuditError> {
        Ok(Self {
            epoch: 0,
            seq: 0,
            hash: GENESIS.parse().map_err(AuditError::Invalid)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Event<'a> {
    pub principal: &'a str,
    pub action: &'a str,
    pub subject: Option<String>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
    active_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotation {
    from: Head,
    next: Head,
}

struct WalkBytes<'a> {
    path: &'a Path,
    bytes: &'a [u8],
    base: Head,
    stop_at: u64,
    expected: Option<&'a Head>,
}

fn digest(line: &[u8]) -> Result<ChainHash, AuditError> {
    blake3::hash(line)
        .to_hex()
        .to_string()
        .parse()
        .map_err(AuditError::Invalid)
}

impl AuditLog {
    #[must_use]
    pub fn at(dirs: &Dirs) -> Self {
        Self {
            path: dirs.state.join("audit.jsonl"),
            active_limit: ACTIVE_LIMIT,
        }
    }

    fn head_path(&self) -> PathBuf {
        self.path.with_extension("head")
    }

    fn base_path(&self) -> PathBuf {
        self.path.with_extension("base")
    }

    fn rotation_path(&self) -> PathBuf {
        self.path.with_extension("rotation")
    }

    fn archive_path(&self, epoch: u64) -> PathBuf {
        self.path.with_file_name(format!("audit-{epoch}.jsonl"))
    }

    fn archive_base_path(&self, epoch: u64) -> PathBuf {
        self.path.with_file_name(format!("audit-{epoch}.base"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, event: Event<'_>) -> Result<(), AuditError> {
        let Event {
            principal,
            action,
            subject,
            verdict,
        } = event;
        let io = |step| {
            let path = self.path.clone();
            move |source| {
                AuditError::Io(crate::failure::IoFailure {
                    action: step,
                    path,
                    source,
                })
            }
        };
        self.with_lock(|| {
            self.finish_rotation()?;
            let mut head = match self.repair()? {
                Some(advanced) => {
                    state_file::write_json(&self.head_path(), &advanced)?;
                    advanced
                }
                None => match state_file::read_json::<Head>(&self.head_path())? {
                    Some(head) => head,
                    None => Head::genesis()?,
                },
            };
            if self.active_len()? >= self.active_limit && head.seq > self.base()?.seq {
                head = self.rotate(&head)?;
            }
            let entry = Entry {
                epoch: head.epoch,
                seq: head.seq.saturating_add(1),
                at: Timestamp::observe(),
                prev: head.hash.to_string(),
                principal: principal.to_owned(),
                action: action.to_owned(),
                subject,
                verdict,
            };
            let line = serde_json::to_vec(&entry).map_err(AuditError::Encode)?;
            let mut file = state_file::open_append(&self.path)?;
            crate::faults::at("audit::append", &self.path)
                .and_then(|()| file.write_all(&line))
                .and_then(|()| file.write_all(b"\n"))
                .map_err(io("appending to"))?;
            crate::faults::at("audit::sync", &self.path)
                .and_then(|()| file.sync_all())
                .map_err(io("syncing"))?;
            state_file::write_json(
                &self.head_path(),
                &Head {
                    epoch: entry.epoch,
                    seq: entry.seq,
                    hash: digest(&line)?,
                },
            )?;
            Ok(())
        })
    }

    fn active_len(&self) -> Result<u64, AuditError> {
        match std::fs::metadata(&self.path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(source) => Err(AuditError::Io(crate::failure::IoFailure {
                action: "measuring",
                path: self.path.clone(),
                source,
            })),
        }
    }

    fn base(&self) -> Result<Head, AuditError> {
        Ok(state_file::read_json(&self.base_path())?.unwrap_or(Head::genesis()?))
    }

    fn rotate(&self, from: &Head) -> Result<Head, AuditError> {
        let next = Head {
            epoch: from.epoch.saturating_add(1),
            seq: from.seq,
            hash: from.hash.clone(),
        };
        state_file::write_json(
            &self.rotation_path(),
            &Rotation {
                from: from.clone(),
                next: next.clone(),
            },
        )?;
        self.finish_rotation()?;
        Ok(next)
    }

    fn finish_rotation(&self) -> Result<(), AuditError> {
        let Some(rotation) = state_file::read_json::<Rotation>(&self.rotation_path())? else {
            return Ok(());
        };
        let archive = self.archive_path(rotation.from.epoch);
        if state_file::read_bytes(&archive)?.is_none() {
            let active = state_file::read_bytes(&self.path)?.unwrap_or_default();
            state_file::write_bytes(&archive, &active)?;
        }
        let archive_base = self.archive_base_path(rotation.from.epoch);
        if state_file::read_bytes(&archive_base)?.is_none() {
            state_file::write_json(&archive_base, &self.base()?)?;
        }
        state_file::write_bytes(&self.path, b"")?;
        state_file::write_json(&self.base_path(), &rotation.next)?;
        state_file::write_json(&self.head_path(), &rotation.next)?;
        Ok(state_file::remove_file(&self.rotation_path())?)
    }

    fn lock(&self) -> Result<OsLock, AuditError> {
        let mut lock_path = self.path.as_os_str().to_owned();
        lock_path.push(".lock");
        OsLock::exclusive(Path::new(&lock_path)).map_err(|e| {
            AuditError::Io(crate::failure::IoFailure {
                action: "locking",
                path: self.path.clone(),
                source: std::io::Error::other(e.to_string()),
            })
        })
    }

    fn unlock(&self, lock: OsLock) -> Result<(), AuditError> {
        lock.release().map_err(|error| {
            AuditError::Io(crate::failure::IoFailure {
                action: "unlocking",
                path: self.path.clone(),
                source: std::io::Error::other(error.to_string()),
            })
        })
    }

    fn with_lock<T>(&self, work: impl FnOnce() -> Result<T, AuditError>) -> Result<T, AuditError> {
        let lock = self.lock()?;
        let result = work();
        let released = self.unlock(lock);
        match (result, released) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    fn repair(&self) -> Result<Option<Head>, AuditError> {
        let Some(mut bytes) = state_file::read_bytes(&self.path)? else {
            return Ok(None);
        };
        if bytes.last().is_some_and(|b| *b != b'\n') {
            let keep = bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |at| at.saturating_add(1));
            state_file::cut_to(&self.path, crate::domain::len_u64(keep))?;
            bytes.truncate(keep);
        }
        let head = match state_file::read_json::<Head>(&self.head_path()) {
            Ok(Some(head)) => head,
            Ok(None) => self.base()?,
            Err(_unreadable_head_is_for_walk_to_report) => return Ok(None),
        };
        let base = self.base()?;
        if head.epoch != base.epoch || head.seq < base.seq {
            return Ok(None);
        }
        let lines: Vec<&[u8]> = bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        if crate::domain::len_u64(lines.len())
            != head.seq.saturating_sub(base.seq).saturating_add(1)
        {
            return Ok(None);
        }
        let Some(last) = lines.last() else {
            return Ok(None);
        };
        let Ok(entry) = crate::ingress::json::<Entry>(last) else {
            return Ok(None);
        };
        if entry.epoch == head.epoch
            && entry.seq == head.seq.saturating_add(1)
            && entry.prev == head.hash.as_str()
        {
            return Ok(Some(Head {
                epoch: entry.epoch,
                seq: entry.seq,
                hash: digest(last)?,
            }));
        }
        Ok(None)
    }

    pub fn verify(&self) -> Result<u64, AuditError> {
        self.with_lock(|| {
            self.finish_rotation()?;
            let current = self.walk_locked(u64::MAX)?.0;
            let base = self.base()?;
            let mut expected = Head::genesis()?;
            for epoch in 0..base.epoch {
                let archive_base: Head = state_file::read_json(&self.archive_base_path(epoch))?
                    .ok_or_else(|| AuditError::Broken {
                        path: self.archive_base_path(epoch),
                        line: expected.seq,
                        why: "an archive base is missing",
                    })?;
                if archive_base != expected {
                    return Err(AuditError::Broken {
                        path: self.archive_base_path(epoch),
                        line: expected.seq,
                        why: "an archive does not extend the previous epoch",
                    });
                }
                let next = if epoch.saturating_add(1) == base.epoch {
                    base.clone()
                } else {
                    state_file::read_json(&self.archive_base_path(epoch.saturating_add(1)))?
                        .ok_or_else(|| AuditError::Broken {
                            path: self.archive_base_path(epoch.saturating_add(1)),
                            line: expected.seq,
                            why: "an archive base is missing",
                        })?
                };
                self.walk_archive(epoch, u64::MAX, &next)?;
                expected = next;
            }
            if expected.hash != base.hash || expected.seq != base.seq {
                return Err(AuditError::Broken {
                    path: self.base_path(),
                    line: base.seq,
                    why: "the active epoch does not extend its archives",
                });
            }
            Ok(current.seq)
        })
    }

    pub fn head(&self) -> Result<Head, AuditError> {
        Ok(self.walk(u64::MAX)?.0)
    }

    pub fn hash_at(&self, epoch: u64, seq: u64) -> Result<Option<ChainHash>, AuditError> {
        self.with_lock(|| {
            self.finish_rotation()?;
            let base = self.base()?;
            let reached = match epoch.cmp(&base.epoch) {
                std::cmp::Ordering::Equal => self.walk_repaired(seq, None)?.0,
                std::cmp::Ordering::Less => {
                    let next = if epoch.saturating_add(1) == base.epoch {
                        base
                    } else {
                        let Some(next) = state_file::read_json(
                            &self.archive_base_path(epoch.saturating_add(1)),
                        )?
                        else {
                            return Ok(None);
                        };
                        next
                    };
                    self.walk_archive(epoch, seq, &next)?.0
                }
                std::cmp::Ordering::Greater => base,
            };
            Ok((reached.epoch == epoch && reached.seq == seq).then_some(reached.hash))
        })
    }

    fn walk(&self, stop_at: u64) -> Result<(Head, u64), AuditError> {
        self.with_lock(|| {
            self.finish_rotation()?;
            self.walk_locked(stop_at)
        })
    }

    fn walk_locked(&self, stop_at: u64) -> Result<(Head, u64), AuditError> {
        let unrecorded = match self.repair()? {
            Some(advanced) => match state_file::write_json(&self.head_path(), &advanced) {
                Ok(()) => None,
                Err(_full_disk_still_lets_it_be_read) => Some(advanced),
            },
            None => None,
        };
        self.walk_repaired(stop_at, unrecorded.as_ref())
    }

    fn walk_repaired(
        &self,
        stop_at: u64,
        unrecorded: Option<&Head>,
    ) -> Result<(Head, u64), AuditError> {
        let base = self.base()?;
        let recorded = || match unrecorded {
            Some(head) => Ok(Some(head.clone())),
            None => state_file::read_json::<Head>(&self.head_path()),
        };
        let Some(bytes) = state_file::read_bytes(&self.path)? else {
            return match recorded()? {
                None if base == Head::genesis()? => Ok((base, 0)),
                Some(_) => Err(AuditError::Broken {
                    path: self.path.clone(),
                    line: 0,
                    why: "entries were removed from the end",
                }),
                None => Err(AuditError::Broken {
                    path: self.path.clone(),
                    line: base.seq,
                    why: "the active epoch is missing",
                }),
            };
        };
        let recorded = recorded()?;
        Self::walk_bytes(WalkBytes {
            path: &self.path,
            bytes: &bytes,
            base,
            stop_at,
            expected: recorded.as_ref(),
        })
    }

    fn walk_archive(
        &self,
        epoch: u64,
        stop_at: u64,
        expected: &Head,
    ) -> Result<(Head, u64), AuditError> {
        let path = self.archive_path(epoch);
        let bytes = state_file::read_bytes(&path)?.ok_or_else(|| AuditError::Broken {
            path: path.clone(),
            line: expected.seq,
            why: "an archive is missing",
        })?;
        let base: Head =
            state_file::read_json(&self.archive_base_path(epoch))?.ok_or_else(|| {
                AuditError::Broken {
                    path: self.archive_base_path(epoch),
                    line: expected.seq,
                    why: "an archive base is missing",
                }
            })?;
        Self::walk_bytes(WalkBytes {
            path: &path,
            bytes: &bytes,
            base,
            stop_at,
            expected: Some(expected),
        })
    }

    fn walk_bytes(walk: WalkBytes<'_>) -> Result<(Head, u64), AuditError> {
        let WalkBytes {
            path,
            bytes,
            base,
            stop_at,
            expected,
        } = walk;
        let broken = |line: u64, why| AuditError::Broken {
            path: path.to_path_buf(),
            line,
            why,
        };
        let mut reached = base;
        if stop_at == reached.seq {
            return Ok((reached.clone(), reached.seq));
        }
        for raw in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let next = reached.seq.saturating_add(1);
            let entry: Entry =
                crate::ingress::json(raw).map_err(|_malformed| broken(next, "malformed entry"))?;
            if entry.epoch != reached.epoch || entry.seq != next {
                return Err(broken(next, "sequence gap"));
            }
            if entry.prev != reached.hash.as_str() {
                return Err(broken(next, "hash chain mismatch"));
            }
            reached = Head {
                epoch: entry.epoch,
                seq: next,
                hash: digest(raw)?,
            };
            if reached.seq == stop_at {
                return Ok((reached, stop_at));
            }
        }
        let count = reached.seq;
        match expected {
            Some(head)
                if head.seq == reached.seq
                    && head.hash == reached.hash
                    && matches!(
                        head.epoch,
                        epoch if epoch == reached.epoch
                            || epoch == reached.epoch.saturating_add(1)
                    ) =>
            {
                Ok((reached, count))
            }
            None if bytes.is_empty() => Ok((reached, count)),
            Some(_) | None => Err(broken(count, "entries were removed from the end")),
        }
    }

    pub fn tail(&self, lines: usize) -> Result<Vec<Entry>, AuditError> {
        if lines == 0 {
            return Ok(Vec::new());
        }
        self.with_lock(|| {
            self.finish_rotation()?;
            let mut entries = std::collections::VecDeque::with_capacity(lines);
            let base = self.base()?;
            for epoch in 0..=base.epoch {
                let path = if epoch == base.epoch {
                    self.path.clone()
                } else {
                    self.archive_path(epoch)
                };
                let Some(bytes) = state_file::read_bytes(&path)? else {
                    continue;
                };
                for raw in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                    let entry: Entry =
                        crate::ingress::json(raw).map_err(|_malformed| AuditError::Broken {
                            path: path.clone(),
                            line: 0,
                            why: "malformed entry",
                        })?;
                    if entries.len() == lines {
                        entries.pop_front();
                    }
                    entries.push_back(entry);
                }
            }
            Ok(entries.into_iter().collect())
        })
    }
}

impl crate::ingress::Ingress for Entry {}
impl crate::ingress::Ingress for Head {}
impl crate::ingress::Ingress for Rotation {}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build their fixtures directly on disk"
)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> Dirs {
        Dirs::for_test(root)
    }

    fn event(action: &str) -> Event<'_> {
        Event {
            principal: "mac (abcd)",
            action,
            subject: None,
            verdict: Verdict::Allowed,
        }
    }

    fn recorded(actions: &[&str]) -> (tempfile::TempDir, AuditLog) {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        for action in actions {
            log.record(event(action)).unwrap();
        }
        (tmp, log)
    }

    #[test]
    fn records_from_before_epochs_existed_belong_to_the_first_epoch() {
        let head: Head = serde_json::from_value(serde_json::json!({
            "seq": 0,
            "hash": GENESIS
        }))
        .unwrap();
        assert_eq!(head.epoch, 0);
        let mut value = serde_json::to_value(Entry {
            epoch: 0,
            seq: 1,
            at: Timestamp::at_millis(1),
            prev: GENESIS.to_owned(),
            principal: "owner".to_owned(),
            action: "list".to_owned(),
            subject: None,
            verdict: Verdict::Allowed,
        })
        .unwrap();
        value.as_object_mut().unwrap().remove("epoch");
        let entry: Entry = serde_json::from_value(value).unwrap();
        assert_eq!(entry.epoch, 0);
    }

    #[test]
    fn the_chain_detects_edits_and_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let log = AuditLog::at(&dirs);
        assert_eq!(log.verify().unwrap(), 0);
        for action in ["submit", "kill", "get"] {
            log.record(Event {
                principal: "mac (abcd)",
                action,
                subject: Some("06GD".into()),
                verdict: Verdict::Allowed,
            })
            .unwrap();
        }
        assert_eq!(log.verify().unwrap(), 3);
        let head = log.head().unwrap();
        assert_eq!(head.seq, 3);
        assert_eq!(log.hash_at(0, 3).unwrap().unwrap(), head.hash);
        let at_one = log.hash_at(0, 1).unwrap().unwrap();
        assert_ne!(at_one, head.hash);
        assert!(log.hash_at(0, 4).unwrap().is_none());
        assert_eq!(log.hash_at(0, 0).unwrap().unwrap().as_str(), GENESIS);
        assert!(log.tail(0).unwrap().is_empty());
        assert_eq!(log.tail(2).unwrap().len(), 2);
        let text = std::fs::read_to_string(log.path()).unwrap();
        std::fs::write(log.path(), text.replacen("\"kill\"", "\"list\"", 1)).unwrap();
        assert!(matches!(
            log.verify(),
            Err(AuditError::Broken { line: 3, .. })
        ));
        let mut lines: Vec<&str> = text.lines().collect();
        lines.pop();
        std::fs::write(log.path(), format!("{}\n", lines.join("\n"))).unwrap();
        assert!(matches!(log.verify(), Err(AuditError::Broken { .. })));
    }

    #[test]
    fn a_removed_entry_is_caught_even_when_the_chain_and_head_are_relinked() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&Dirs::for_test(tmp.path()));
        for action in ["submit", "kill", "get"] {
            log.record(event(action)).unwrap();
        }
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let [first, _, third] = lines.as_slice() else {
            panic!("expected three entries");
        };
        let mut relinked: Entry = serde_json::from_str(third).unwrap();
        relinked.prev = digest(first.as_bytes()).unwrap().to_string();
        let relinked = serde_json::to_vec(&relinked).unwrap();
        let mut forged = first.as_bytes().to_vec();
        forged.push(b'\n');
        forged.extend_from_slice(&relinked);
        forged.push(b'\n');
        std::fs::write(log.path(), forged).unwrap();
        state_file::write_json(
            &log.head_path(),
            &Head {
                epoch: 0,
                seq: 2,
                hash: digest(&relinked).unwrap(),
            },
        )
        .unwrap();
        assert!(matches!(
            log.verify(),
            Err(AuditError::Broken {
                line: 2,
                why: "sequence gap",
                ..
            })
        ));
    }

    #[test]
    fn damage_anywhere_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        state_file::write_bytes(&log.head_path(), b"not a head").unwrap();
        log.verify().unwrap_err();
        log.record(event("submit")).unwrap_err();

        std::fs::remove_file(log.head_path()).unwrap();
        log.record(event("submit")).unwrap();
        log.record(event("kill")).unwrap();
        state_file::write_bytes(&log.head_path(), b"not a head").unwrap();
        log.verify().unwrap_err();
        log.head().unwrap_err();
        log.hash_at(0, 5).unwrap_err();

        std::fs::remove_file(log.head_path()).unwrap();
        state_file::write_bytes(log.path(), b"not an entry\n").unwrap();
        let damaged = log.verify();
        assert!(
            matches!(
                damaged,
                Err(AuditError::Broken {
                    why: "malformed entry",
                    ..
                })
            ),
            "{damaged:?}"
        );
        log.tail(1).unwrap_err();
    }

    #[test]
    fn a_full_active_log_starts_a_verifiable_archive_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = AuditLog::at(&dirs(tmp.path()));
        log.active_limit = 1;
        for action in ["submit", "kill", "get"] {
            log.record(event(action)).unwrap();
        }
        let head = log.head().unwrap();
        assert_eq!((head.epoch, head.seq), (2, 3));
        assert!(log.archive_path(0).is_file());
        assert!(log.archive_path(1).is_file());
        assert!(log.hash_at(0, 1).unwrap().is_some());
        assert!(log.hash_at(1, 2).unwrap().is_some());
        assert_eq!(log.hash_at(2, 3).unwrap(), Some(head.hash));
        assert_eq!(log.verify().unwrap(), 3);
        assert_eq!(log.tail(3).unwrap().len(), 3);
    }

    #[test]
    fn an_unusable_log_or_lock_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
        std::fs::remove_file(log.path()).unwrap();
        std::fs::remove_file(log.head_path()).unwrap();
        std::fs::create_dir_all(log.path()).unwrap();
        log.tail(1).unwrap_err();
        log.verify().unwrap_err();
        assert!(matches!(
            log.record(event("submit")),
            Err(AuditError::State(_))
        ));

        let other = tempfile::tempdir().unwrap();
        let unlockable = AuditLog::at(&dirs(other.path()));
        unlockable.record(event("submit")).unwrap();
        let mut lock = unlockable.path().as_os_str().to_owned();
        lock.push(".lock");
        std::fs::remove_file(Path::new(&lock)).unwrap();
        std::fs::create_dir_all(Path::new(&lock)).unwrap();
        assert!(matches!(
            unlockable.record(event("submit")),
            Err(AuditError::Io(crate::failure::IoFailure {
                action: "locking",
                ..
            }))
        ));
    }

    #[test]
    fn an_empty_log_is_fresh_but_a_missing_head_after_entries_is_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
        state_file::write_bytes(log.path(), b"").unwrap();
        std::fs::remove_file(log.head_path()).unwrap();
        assert_eq!(log.verify().unwrap(), 0);

        let other = tempfile::tempdir().unwrap();
        let written = AuditLog::at(&dirs(other.path()));
        written.record(event("submit")).unwrap();
        written.record(event("kill")).unwrap();
        std::fs::remove_file(written.head_path()).unwrap();
        assert!(matches!(
            written.verify(),
            Err(AuditError::Broken {
                why: "entries were removed from the end",
                ..
            })
        ));
    }

    #[test]
    fn a_crash_between_the_entry_and_the_head_heals_on_the_next_read_or_write() {
        let (_tmp, log) = recorded(&["submit"]);
        let head_after_one = std::fs::read(log.head_path()).unwrap();
        log.record(event("kill")).unwrap();
        std::fs::write(log.head_path(), &head_after_one).unwrap();
        assert_eq!(log.verify().unwrap(), 2);
        log.record(event("get")).unwrap();
        assert_eq!(log.verify().unwrap(), 3);

        let first = tempfile::tempdir().unwrap();
        let fresh = AuditLog::at(&dirs(first.path()));
        fresh.record(event("submit")).unwrap();
        std::fs::remove_file(fresh.head_path()).unwrap();
        assert_eq!(fresh.head().unwrap().seq, 1);
    }

    #[test]
    fn a_crash_partway_through_an_entry_heals_and_loses_nothing_recorded() {
        let (_tmp, log) = recorded(&["submit", "kill"]);
        let whole = std::fs::read(log.path()).unwrap();
        let mut torn = whole.clone();
        torn.extend_from_slice(br#"{"seq":3,"at":"#);
        std::fs::write(log.path(), &torn).unwrap();
        assert_eq!(log.verify().unwrap(), 2);
        assert_eq!(std::fs::read(log.path()).unwrap(), whole);
        log.record(event("get")).unwrap();
        assert_eq!(log.verify().unwrap(), 3);
    }

    #[test]
    fn an_entry_that_does_not_extend_the_head_is_never_taken_as_the_new_head() {
        let (_tmp, log) = recorded(&["submit"]);
        let head_after_one = std::fs::read(log.head_path()).unwrap();
        log.record(event("kill")).unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        let (first, second) = text.trim_end().split_once('\n').unwrap();
        let mut forged: Entry = serde_json::from_str(second).unwrap();
        forged.prev = "0".repeat(64);
        let forged = serde_json::to_string(&forged).unwrap();
        std::fs::write(log.path(), format!("{first}\n{forged}\n")).unwrap();
        std::fs::write(log.head_path(), &head_after_one).unwrap();
        assert!(matches!(log.verify(), Err(AuditError::Broken { .. })));
        assert_eq!(std::fs::read(log.head_path()).unwrap(), head_after_one);

        let mut renumbered: Entry = serde_json::from_str(second).unwrap();
        renumbered.seq = 5;
        let renumbered = serde_json::to_string(&renumbered).unwrap();
        std::fs::write(log.path(), format!("{first}\n{renumbered}\n")).unwrap();
        assert!(matches!(log.verify(), Err(AuditError::Broken { .. })));
        assert_eq!(std::fs::read(log.head_path()).unwrap(), head_after_one);
    }

    fn tag(tmp: &tempfile::TempDir) -> String {
        tmp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_failing_disk_is_an_error_at_every_step_and_the_log_heals_after() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
        let tag = tag(&tmp);
        for site in [
            "audit::append",
            "audit::sync",
            "state_file::append",
            "state_file::lock",
            "state_file::read",
        ] {
            let faults = crate::faults::inject(&[(site, &tag)]);
            log.record(event("kill")).unwrap_err();
            drop(faults);
        }
        let unreadable = crate::faults::inject(&[("state_file::read", &tag)]);
        log.verify().unwrap_err();
        log.head().unwrap_err();
        log.hash_at(0, 1).unwrap_err();
        drop(unreadable);
        let entries = log.path().display().to_string();
        let unreadable_on_the_walk = crate::faults::inject_after("state_file::read", 2, &entries);
        log.verify().unwrap_err();
        drop(unreadable_on_the_walk);
        let written = log.verify().unwrap();
        let head_tag = log.head_path().display().to_string();
        let headless = crate::faults::inject(&[("state_file::write", &head_tag)]);
        log.record(event("get")).unwrap_err();
        drop(headless);
        assert_eq!(log.verify().unwrap(), written.saturating_add(1));

        let mut torn = std::fs::read(log.path()).unwrap();
        torn.extend_from_slice(b"{\"seq\":");
        std::fs::write(log.path(), &torn).unwrap();
        let uncuttable = crate::faults::inject(&[("state_file::cut", &tag)]);
        log.verify().unwrap_err();
        drop(uncuttable);
        assert_eq!(log.verify().unwrap(), written.saturating_add(1));
    }

    #[test]
    fn a_read_that_cannot_lock_fails_and_one_on_a_full_disk_still_reads_and_heals_later() {
        let (_tmp, log) = recorded(&["submit"]);
        let head_after_one = std::fs::read(log.head_path()).unwrap();
        log.record(event("kill")).unwrap();
        let lock = format!("{}.lock", log.path().display());
        {
            let _faults = crate::faults::inject(&[("state_file::lock", &lock)]);
            log.verify().unwrap_err();
        }
        std::fs::write(log.head_path(), &head_after_one).unwrap();
        let head = log.head_path().display().to_string();
        {
            let _faults = crate::faults::inject(&[("state_file::write", &head)]);
            assert_eq!(log.verify().unwrap(), 2);
            assert_eq!(log.hash_at(0, 2).unwrap(), Some(log.head().unwrap().hash));
            assert_eq!(std::fs::read(log.head_path()).unwrap(), head_after_one);
            log.record(event("get")).unwrap_err();
        }
        assert_eq!(log.verify().unwrap(), 2);
        assert_ne!(std::fs::read(log.head_path()).unwrap(), head_after_one);
        std::fs::write(log.head_path(), &head_after_one).unwrap();
        log.record(event("get")).unwrap();
        assert_eq!(log.verify().unwrap(), 3);
    }

    #[test]
    fn two_entries_past_the_head_are_not_healed_but_reported() {
        let (_tmp, log) = recorded(&["submit"]);
        let head_after_one = std::fs::read(log.head_path()).unwrap();
        log.record(event("kill")).unwrap();
        log.record(event("get")).unwrap();
        std::fs::write(log.head_path(), &head_after_one).unwrap();
        assert!(matches!(log.verify(), Err(AuditError::Broken { .. })));
    }
}
