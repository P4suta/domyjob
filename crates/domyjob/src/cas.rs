use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{BlobId, RelPath};
use crate::snapshot::Manifest;

#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("blob {expected} arrived as {actual}")]
    Corrupt { expected: BlobId, actual: BlobId },
    #[error("blob {blob} announces {size} bytes, more than a blob may hold")]
    TooLarge { blob: BlobId, size: u64 },
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

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> CasError + use<> {
    let path = path.to_path_buf();
    move |source| CasError::Io {
        action,
        path,
        source,
    }
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
            Err(e) => Err(io("checking", &path)(e)),
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
                staged.discard()?;
                Err(CasError::Corrupt {
                    expected: blob.clone(),
                    actual,
                })
            }
            (Err(error), _) => {
                staged.discard()?;
                Err(io("receiving", &path)(error))
            }
        }
    }

    pub fn get(&self, blob: &BlobId) -> Result<Vec<u8>, CasError> {
        let path = self.path(blob);
        crate::faults::at("cas::read", &path).map_err(io("reading", &path))?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(CasError::Missing(blob.clone()));
            }
            Err(e) => return Err(io("reading", &path)(e)),
        };
        if &BlobId::of(&bytes) != blob {
            crate::state_file::remove_file(&path)?;
            return Err(CasError::Damaged(blob.clone()));
        }
        Ok(bytes)
    }

    pub fn stored(&self) -> Result<Vec<BlobId>, CasError> {
        crate::faults::at("cas::list", &self.root).map_err(io("listing", &self.root))?;
        let mut out = Vec::new();
        let fans = match std::fs::read_dir(&self.root) {
            Ok(fans) => fans,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(io("listing", &self.root)(e)),
        };
        for fan in fans {
            let fan = fan.map_err(io("listing", &self.root))?;
            let Ok(inner) = std::fs::read_dir(fan.path()) else {
                continue;
            };
            for blob in inner.flatten() {
                let name = format!(
                    "{}{}",
                    fan.file_name().to_string_lossy(),
                    blob.file_name().to_string_lossy()
                );
                match name.parse::<BlobId>() {
                    Ok(id) => out.push(id),
                    Err(_staging_or_foreign) => {}
                }
            }
        }
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
}
