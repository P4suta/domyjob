use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::domain::ChainHash;
use crate::lock::OsLock;
use crate::paths::Dirs;
use crate::state_file::{self, StateError};

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
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
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub seq: u64,
    pub at: Timestamp,
    pub prev: String,
    pub principal: String,
    pub action: String,
    pub subject: Option<String>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Head {
    pub seq: u64,
    pub hash: ChainHash,
}

impl Head {
    fn genesis() -> Result<Self, AuditError> {
        Ok(Self {
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
        }
    }

    fn head_path(&self) -> PathBuf {
        self.path.with_extension("head")
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
            move |source| AuditError::Io {
                action: step,
                path,
                source,
            }
        };
        let lock = self.lock()?;
        let head = match self.repair()? {
            Some(advanced) => {
                state_file::write_json(&self.head_path(), &advanced)?;
                advanced
            }
            None => match state_file::read_json::<Head>(&self.head_path())? {
                Some(head) => head,
                None => Head::genesis()?,
            },
        };
        let entry = Entry {
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
                seq: entry.seq,
                hash: digest(&line)?,
            },
        )?;
        lock.release()
            .map_err(|e| io("unlocking")(std::io::Error::other(e.to_string())))
    }

    fn lock(&self) -> Result<OsLock, AuditError> {
        let mut lock_path = self.path.as_os_str().to_owned();
        lock_path.push(".lock");
        OsLock::exclusive(Path::new(&lock_path)).map_err(|e| AuditError::Io {
            action: "locking",
            path: self.path.clone(),
            source: std::io::Error::other(e.to_string()),
        })
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
            Ok(None) => Head::genesis()?,
            Err(_unreadable_head_is_for_walk_to_report) => return Ok(None),
        };
        let lines: Vec<&[u8]> = bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        if crate::domain::len_u64(lines.len()) != head.seq.saturating_add(1) {
            return Ok(None);
        }
        let Some(last) = lines.last() else {
            return Ok(None);
        };
        let Ok(entry) = crate::ingress::json::<Entry>(last) else {
            return Ok(None);
        };
        if entry.seq == head.seq.saturating_add(1) && entry.prev == head.hash.as_str() {
            return Ok(Some(Head {
                seq: entry.seq,
                hash: digest(last)?,
            }));
        }
        Ok(None)
    }

    pub fn verify(&self) -> Result<u64, AuditError> {
        Ok(self.walk(u64::MAX)?.0.seq)
    }

    pub fn head(&self) -> Result<Head, AuditError> {
        Ok(self.walk(u64::MAX)?.0)
    }

    pub fn hash_at(&self, seq: u64) -> Result<Option<ChainHash>, AuditError> {
        let (reached, _) = self.walk(seq)?;
        Ok((reached.seq == seq).then_some(reached.hash))
    }

    fn walk(&self, stop_at: u64) -> Result<(Head, u64), AuditError> {
        let lock = self.lock()?;
        let unrecorded = match self.repair()? {
            Some(advanced) => match state_file::write_json(&self.head_path(), &advanced) {
                Ok(()) => None,
                Err(_full_disk_still_lets_it_be_read) => Some(advanced),
            },
            None => None,
        };
        let walked = self.walk_repaired(stop_at, unrecorded.as_ref());
        lock.release().map_err(|e| AuditError::Io {
            action: "unlocking",
            path: self.path.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
        walked
    }

    fn walk_repaired(
        &self,
        stop_at: u64,
        unrecorded: Option<&Head>,
    ) -> Result<(Head, u64), AuditError> {
        let genesis = Head::genesis()?;
        let recorded = || match unrecorded {
            Some(head) => Ok(Some(head.clone())),
            None => state_file::read_json::<Head>(&self.head_path()),
        };
        let Some(bytes) = state_file::read_bytes(&self.path)? else {
            return match recorded()? {
                None => Ok((genesis, 0)),
                Some(_) => Err(AuditError::Broken {
                    path: self.path.clone(),
                    line: 0,
                    why: "entries were removed from the end",
                }),
            };
        };
        let broken = |line: u64, why| AuditError::Broken {
            path: self.path.clone(),
            line,
            why,
        };
        let mut reached = genesis;
        if stop_at == 0 {
            return Ok((reached, 0));
        }
        for raw in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let next = reached.seq.saturating_add(1);
            let entry: Entry =
                crate::ingress::json(raw).map_err(|_malformed| broken(next, "malformed entry"))?;
            if entry.seq != next {
                return Err(broken(next, "sequence gap"));
            }
            if entry.prev != reached.hash.as_str() {
                return Err(broken(next, "hash chain mismatch"));
            }
            reached = Head {
                seq: next,
                hash: digest(raw)?,
            };
            if reached.seq == stop_at {
                return Ok((reached, stop_at));
            }
        }
        let count = reached.seq;
        match recorded()? {
            Some(head) if head == reached => Ok((reached, count)),
            None if count == 0 => Ok((reached, 0)),
            Some(_) | None => Err(broken(count, "entries were removed from the end")),
        }
    }

    pub fn tail(&self, lines: usize) -> Result<Vec<Entry>, AuditError> {
        let Some(bytes) = state_file::read_bytes(&self.path)? else {
            return Ok(Vec::new());
        };
        let mut entries = std::collections::VecDeque::with_capacity(lines);
        for (index, raw) in bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .enumerate()
        {
            let entry: Entry =
                crate::ingress::json(raw).map_err(|_malformed| AuditError::Broken {
                    path: self.path.clone(),
                    line: crate::domain::len_u64(index.saturating_add(1)),
                    why: "malformed entry",
                })?;
            if entries.len() == lines {
                entries.pop_front();
            }
            entries.push_back(entry);
        }
        Ok(entries.into_iter().collect())
    }
}

impl crate::ingress::Ingress for Entry {}
impl crate::ingress::Ingress for Head {}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build their fixtures directly on disk"
)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> Dirs {
        Dirs {
            home: root.into(),
            state: root.join("s"),
            config: root.join("c"),
            cache: root.join("k"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        }
    }

    fn event(action: &str) -> Event<'_> {
        Event {
            principal: "mac (abcd)",
            action,
            subject: None,
            verdict: Verdict::Allowed,
        }
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
        assert_eq!(log.hash_at(3).unwrap().unwrap(), head.hash);
        let at_one = log.hash_at(1).unwrap().unwrap();
        assert_ne!(at_one, head.hash);
        assert!(log.hash_at(4).unwrap().is_none());
        assert_eq!(log.hash_at(0).unwrap().unwrap().as_str(), GENESIS);
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
        let log = AuditLog::at(&dirs(tmp.path()));
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
        log.hash_at(5).unwrap_err();

        state_file::write_bytes(log.path(), b"not an entry\n").unwrap();
        assert!(matches!(
            log.verify(),
            Err(AuditError::Broken {
                why: "malformed entry",
                ..
            })
        ));
        log.tail(1).unwrap_err();
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
            Err(AuditError::Io {
                action: "locking",
                ..
            })
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
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
        log.record(event("kill")).unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
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
        log.hash_at(1).unwrap_err();
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
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
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
            assert_eq!(log.hash_at(2).unwrap(), Some(log.head().unwrap().hash));
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
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::at(&dirs(tmp.path()));
        log.record(event("submit")).unwrap();
        let head_after_one = std::fs::read(log.head_path()).unwrap();
        log.record(event("kill")).unwrap();
        log.record(event("get")).unwrap();
        std::fs::write(log.head_path(), &head_after_one).unwrap();
        assert!(matches!(log.verify(), Err(AuditError::Broken { .. })));
    }
}
