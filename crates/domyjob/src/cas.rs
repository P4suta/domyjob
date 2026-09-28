use crate::failure::io;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::{BlobId, RelPath};
use crate::snapshot::Manifest;

#[cfg(test)]
const STORED_SCAN_BATCH: usize = crate::bounded::DIRECTORY_BATCH.get();

#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error("blob {expected} arrived as {actual}")]
    Corrupt { expected: BlobId, actual: BlobId },
    #[error("blob {blob} announces {size} bytes, more than a blob may hold")]
    TooLarge { blob: BlobId, size: u64 },
    #[error("blob {blob} has {size} bytes, exceeding the {limit}-byte in-memory budget")]
    InMemoryLimit { blob: BlobId, size: u64, limit: u64 },
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error("the manifest cannot be placed here: {0}")]
    Unportable(String),
    #[error("blob {0} is not stored here")]
    Missing(BlobId),
    #[error(
        "blob {0} was damaged on this machine and has been discarded; run the job again and it is sent afresh"
    )]
    Damaged(BlobId),
    #[error("manifest {id} is malformed: {source}")]
    Manifest {
        id: BlobId,
        source: serde_json::Error,
    },
    #[error("encoding workspace state: {0}")]
    Encode(serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    pub fn open(root: PathBuf) -> Result<Self, CasError> {
        crate::state_file::private_dir(&root)?;
        Ok(Self { root })
    }

    fn path(&self, blob: &BlobId) -> PathBuf {
        let (fan, rest) = blob.split();
        self.root.join(fan).join(rest)
    }

    pub fn has(&self, blob: &BlobId) -> Result<bool, CasError> {
        let path = self.path(blob);
        crate::faults::at("cas::check", &path).map_err(io("checking", &path))?;
        match std::fs::metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io("checking", &path)(e).into()),
        }
    }

    pub fn missing(&self, blobs: &[BlobId]) -> Result<Vec<BlobId>, CasError> {
        let mut out = Vec::new();
        for blob in blobs {
            if !self.has(blob)? {
                out.push(blob.clone());
            }
        }
        Ok(out)
    }

    pub fn put(&self, expected: &BlobId, bytes: &[u8]) -> Result<(), CasError> {
        let actual = BlobId::of(bytes);
        if &actual != expected {
            return Err(CasError::Corrupt {
                expected: expected.clone(),
                actual,
            });
        }
        Ok(crate::state_file::write_bytes(&self.path(expected), bytes)?)
    }

    pub fn receive(&self, input: &mut dyn Read, blob: &BlobId, size: u64) -> Result<(), CasError> {
        let path = self.path(blob);
        if size > crate::bounded::BLOB {
            return Err(CasError::TooLarge {
                blob: blob.clone(),
                size,
            });
        }
        let mut staged = crate::state_file::stage(&path)?;
        let mut hasher = blake3::Hasher::new();
        let copied = crate::bounded::exactly(input, size, &mut |chunk| {
            hasher.update(chunk);
            staged.write_all(chunk)
        });
        let actual = BlobId::from_hash(&hasher.finalize());
        match (copied, &actual == blob) {
            (Ok(()), true) => Ok(staged.commit()?),
            (Ok(()), false) => {
                staged.discard();
                Err(CasError::Corrupt {
                    expected: blob.clone(),
                    actual,
                })
            }
            (Err(error), _) => {
                staged.discard();
                Err(io("receiving", &path)(error).into())
            }
        }
    }

    pub fn get(&self, blob: &BlobId) -> Result<Vec<u8>, CasError> {
        let (path, mut file) = self.open_blob(blob)?;
        let size = file.metadata().map_err(io("checking", &path))?.len();
        if size > crate::bounded::IN_MEMORY_FILE {
            return Err(CasError::InMemoryLimit {
                blob: blob.clone(),
                size,
                limit: crate::bounded::IN_MEMORY_FILE,
            });
        }
        let bytes = crate::bounded::to_end(&mut file, crate::bounded::IN_MEMORY_FILE)
            .map_err(io("reading", &path))?;
        if &BlobId::of(&bytes) != blob {
            crate::state_file::remove_file(&path)?;
            return Err(CasError::Damaged(blob.clone()));
        }
        Ok(bytes)
    }

    fn open_blob(&self, blob: &BlobId) -> Result<(PathBuf, std::fs::File), CasError> {
        let path = self.path(blob);
        crate::faults::at("cas::read", &path).map_err(io("reading", &path))?;
        let file =
            crate::state_file::open_read(&path)?.ok_or_else(|| CasError::Missing(blob.clone()))?;
        Ok((path, file))
    }

    pub fn stream(&self, blob: &BlobId, output: &mut dyn Write) -> Result<u64, CasError> {
        let (path, mut file) = self.open_blob(blob)?;
        let mut hasher = blake3::Hasher::new();
        let mut size = 0u64;
        let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
        loop {
            let count = match file.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(io("reading", &path)(error).into()),
            };
            if count == 0 {
                break;
            }
            size = size.saturating_add(crate::domain::len_u64(count));
            if size > crate::bounded::BLOB {
                return Err(CasError::TooLarge {
                    blob: blob.clone(),
                    size,
                });
            }
            let chunk = buffer.get(..count).unwrap_or(&[]);
            hasher.update(chunk);
            output.write_all(chunk).map_err(io("writing", &path))?;
        }
        if &BlobId::from_hash(&hasher.finalize()) != blob {
            crate::state_file::remove_file(&path)?;
            return Err(CasError::Damaged(blob.clone()));
        }
        Ok(size)
    }

    pub fn for_each_stored<E>(&self, visit: impl FnMut(BlobId) -> Result<(), E>) -> Result<(), E>
    where
        E: From<CasError>,
    {
        self.for_each_stored_matching("", visit)
    }

    pub(crate) fn for_each_stored_matching<E>(
        &self,
        wanted: &str,
        mut visit: impl FnMut(BlobId) -> Result<(), E>,
    ) -> Result<(), E>
    where
        E: From<CasError>,
    {
        crate::faults::at("cas::list", &self.root)
            .map_err(io("listing", &self.root))
            .map_err(CasError::from)?;
        let fans = match std::fs::read_dir(&self.root) {
            Ok(fans) => fans,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(CasError::from(io("listing", &self.root)(e)).into()),
        };
        for fan in fans {
            let fan = fan
                .map_err(io("listing", &self.root))
                .map_err(CasError::from)?;
            let fan_name = fan.file_name();
            let Some(prefix) = fan_name.to_str().filter(|name| {
                name.len() == 2
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }) else {
                continue;
            };
            if !prefix.starts_with(wanted) && !wanted.starts_with(prefix) {
                continue;
            }
            let path = fan.path();
            crate::bounded::SortedScan::<BlobId>::new().walk(
                |offer| {
                    crate::faults::at("cas::list", &path)
                        .map_err(io("listing", &path))
                        .map_err(CasError::from)?;
                    let inner = std::fs::read_dir(&path)
                        .map_err(io("listing", &path))
                        .map_err(CasError::from)?;
                    for blob in inner {
                        let blob = blob.map_err(io("listing", &path)).map_err(CasError::from)?;
                        let name = format!("{}{}", prefix, blob.file_name().to_string_lossy());
                        let Ok(id) = name.parse::<BlobId>() else {
                            continue;
                        };
                        if id.as_str().starts_with(wanted) {
                            offer(id);
                        }
                    }
                    Ok::<(), E>(())
                },
                |id| {
                    visit(id)?;
                    Ok(crate::bounded::ScanFlow::Continue)
                },
            )?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn stored(&self) -> Result<Vec<BlobId>, CasError> {
        let mut out = Vec::new();
        self.for_each_stored::<CasError>(|blob| {
            out.push(blob);
            Ok(())
        })?;
        Ok(out)
    }

    pub fn remove(&self, blob: &BlobId) -> Result<(), CasError> {
        Ok(crate::state_file::remove_file(&self.path(blob))?)
    }

    pub fn manifest(&self, id: &BlobId) -> Result<Manifest, CasError> {
        let bytes = self.get(id)?;
        let manifest: Manifest =
            crate::ingress::json(&bytes).map_err(|source| CasError::Manifest {
                id: id.clone(),
                source,
            })?;
        manifest
            .check_portable()
            .map_err(|error| CasError::Unportable(error.to_string()))?;
        Ok(manifest)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applied {
    files: std::collections::BTreeSet<RelPath>,
}

impl Applied {
    pub fn paths(&self) -> impl Iterator<Item = &RelPath> {
        self.files.iter()
    }

    pub fn insert(&mut self, rel: RelPath) {
        self.files.insert(rel);
    }
}

impl crate::ingress::Ingress for Applied {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_stored_is_known_listed_received_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("objects")).unwrap();
        assert!(cas.stored().unwrap().is_empty());
        let kept = BlobId::of(b"kept");
        let other = BlobId::of(b"never stored");
        cas.put(&kept, b"kept").unwrap();
        assert!(cas.has(&kept).unwrap());
        assert_eq!(
            cas.missing(&[kept.clone(), other.clone()]).unwrap(),
            vec![other]
        );
        assert_eq!(cas.stored().unwrap(), vec![kept.clone()]);

        let arriving = BlobId::of(b"over the wire");
        cas.receive(&mut &b"over the wire"[..], &arriving, 13)
            .unwrap();
        assert_eq!(cas.get(&arriving).unwrap(), b"over the wire");
        let lying = BlobId::of(b"what was promised");
        assert!(matches!(
            cas.receive(&mut &b"something else!!"[..], &lying, 16),
            Err(CasError::Corrupt { .. })
        ));
        assert!(matches!(
            cas.receive(&mut &b"short"[..], &lying, 17),
            Err(CasError::Io { .. })
        ));
        assert!(matches!(
            cas.receive(&mut &b""[..], &lying, crate::bounded::BLOB + 1),
            Err(CasError::TooLarge { .. })
        ));
        assert!(!cas.has(&lying).unwrap());
        assert_eq!(cas.stored().unwrap().len(), 2);

        cas.remove(&kept).unwrap();
        assert!(matches!(cas.get(&kept), Err(CasError::Missing(_))));
        assert!(matches!(
            cas.manifest(&arriving),
            Err(CasError::Manifest { .. })
        ));
    }

    #[test]
    fn an_unreadable_fan_cannot_make_a_partial_blob_list_look_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("objects")).unwrap();
        let blob = BlobId::of(b"kept");
        cas.put(&blob, b"kept").unwrap();
        let tag = cas.root.join(blob.split().0).display().to_string();
        let _faults = crate::faults::inject(&[("cas::list", &tag)]);
        assert!(matches!(cas.stored(), Err(CasError::Io { .. })));
    }

    #[test]
    fn deleting_stored_blobs_crosses_batches_without_skipping_a_fan() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("objects")).unwrap();
        for index in 0..=STORED_SCAN_BATCH {
            let id: BlobId = format!("aa{index:062x}").parse().unwrap();
            crate::state_file::write_bytes(&cas.path(&id), b"fixture").unwrap();
        }
        let mut removed = 0;
        cas.for_each_stored::<CasError>(|id| {
            cas.remove(&id)?;
            removed += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(removed, STORED_SCAN_BATCH + 1);
        assert!(cas.stored().unwrap().is_empty());
    }

    #[test]
    fn a_failing_disk_is_an_error_never_an_absence_or_a_success() {
        let tmp = tempfile::tempdir().unwrap();
        let tag = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let root = tmp.path().join("objects");
        {
            let _faults = crate::faults::inject(&[("state_file::dir", &tag)]);
            Cas::open(root.clone()).unwrap_err();
        }
        let cas = Cas::open(root).unwrap();
        let kept = BlobId::of(b"kept");
        cas.put(&kept, b"kept").unwrap();
        {
            let _faults = crate::faults::inject(&[
                ("cas::check", &tag),
                ("cas::read", &tag),
                ("cas::list", &tag),
            ]);
            cas.has(&kept).unwrap_err();
            cas.missing(std::slice::from_ref(&kept)).unwrap_err();
            assert!(matches!(cas.get(&kept), Err(CasError::Io { .. })));
            cas.manifest(&kept).unwrap_err();
            cas.stored().unwrap_err();
        }
        let fresh = BlobId::of(b"fresh");
        {
            let _faults = crate::faults::inject(&[("state_file::write", &tag)]);
            cas.put(&fresh, b"fresh").unwrap_err();
        }
        for site in ["state_file::stage", "state_file::commit"] {
            let _faults = crate::faults::inject(&[(site, &tag)]);
            cas.receive(&mut &b"fresh"[..], &fresh, 5).unwrap_err();
        }
        assert!(!cas.has(&fresh).unwrap());
        {
            let _faults = crate::faults::inject(&[("state_file::remove", &tag)]);
            cas.remove(&kept).unwrap_err();
            let lying = BlobId::of(b"promised");
            cas.receive(&mut &b"not what was promised"[..], &lying, 21)
                .unwrap_err();
            crate::state_file::write_bytes(&cas.path(&kept), b"rotted").unwrap();
            assert!(matches!(cas.get(&kept), Err(CasError::State(_))));
        }
        assert!(matches!(cas.get(&kept), Err(CasError::Damaged(_))));
    }

    #[test]
    fn corrupt_blobs_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("objects")).unwrap();
        let claimed = BlobId::of(b"expected");
        assert!(matches!(
            cas.put(&claimed, b"tampered"),
            Err(CasError::Corrupt { .. })
        ));
        assert!(matches!(cas.get(&claimed), Err(CasError::Missing(_))));
        let good = BlobId::of(b"exact");
        cas.put(&good, b"exact").unwrap();
        crate::state_file::write_bytes(&cas.path(&good), b"rotted").unwrap();
        assert!(matches!(cas.get(&good), Err(CasError::Damaged(_))));
        assert_eq!(
            cas.missing(std::slice::from_ref(&good)).unwrap(),
            vec![good.clone()]
        );
        cas.put(&good, b"exact").unwrap();
        assert_eq!(cas.get(&good).unwrap(), b"exact");
    }

    #[test]
    fn large_blobs_stream_in_fixed_memory_and_are_verified() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("objects")).unwrap();
        let content = vec![b'x'; 200_000];
        let blob = BlobId::of(&content);
        cas.put(&blob, &content).unwrap();
        let mut copied = Vec::new();
        assert_eq!(cas.stream(&blob, &mut copied).unwrap(), 200_000);
        assert_eq!(copied, content);
        crate::state_file::write_bytes(&cas.path(&blob), b"changed").unwrap();
        assert!(matches!(
            cas.stream(&blob, &mut Vec::new()),
            Err(CasError::Damaged(_))
        ));
        assert!(!cas.has(&blob).unwrap());
    }

    #[test]
    fn in_memory_reads_refuse_a_large_blob_before_allocating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("objects")).unwrap();
        let blob = BlobId::of(b"small");
        cas.put(&blob, b"small").unwrap();
        crate::state_file::cut_to(&cas.path(&blob), 67_108_865).unwrap();
        assert!(matches!(
            cas.get(&blob),
            Err(CasError::InMemoryLimit {
                limit: crate::bounded::IN_MEMORY_FILE,
                ..
            })
        ));
    }
}
