use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::file_kind;

const MAX_SOURCE_BYTES: u64 = 67_108_864;
const ROOTS: &[&str] = &["crates", "xtask"];
const FILES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "mise.toml",
    "rust-toolchain.toml",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Directory,
    File,
}

struct Scan<'a, F: FnMut(SourceKind, &Path)> {
    root: &'a Path,
    changed: F,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl<F: FnMut(SourceKind, &Path)> Scan<'_, F> {
    fn file(&mut self, path: &Path) -> io::Result<()> {
        (self.changed)(SourceKind::File, path);
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || file_kind::reparse_point(&metadata) {
            return Err(io::Error::other("a source path is not a regular file"));
        }
        self.bytes = self
            .bytes
            .checked_add(metadata.len())
            .ok_or_else(|| io::Error::other("source size overflowed"))?;
        if self.bytes > MAX_SOURCE_BYTES {
            return Err(io::Error::other("source files exceed 64 MiB"));
        }
        let relative = path
            .strip_prefix(self.root)
            .map_err(|_prefix| io::Error::other("source is outside the checkout"))?;
        for part in relative.components() {
            let text = part
                .as_os_str()
                .to_str()
                .ok_or_else(|| io::Error::other("a source path is not UTF-8"))?;
            self.hasher.update(text.as_bytes());
            self.hasher.update(&[0]);
        }
        self.hasher.update(&metadata.len().to_be_bytes());
        let mut file = fs::File::open(path)?;
        let mut buffer = [0_u8; 16_384];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let chunk = buffer
                .get(..count)
                .ok_or_else(|| io::Error::other("source read exceeded the buffer"))?;
            self.hasher.update(chunk);
        }
        Ok(())
    }

    fn directory(&mut self, path: &Path) -> io::Result<()> {
        (self.changed)(SourceKind::Directory, path);
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || file_kind::reparse_point(&metadata) {
            return Err(io::Error::other("a source path is not a directory"));
        }
        let mut entries = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<PathBuf>>>()?;
        entries.sort();
        for entry in entries {
            let child_metadata = fs::symlink_metadata(&entry)?;
            if child_metadata.is_dir() {
                self.directory(&entry)?;
            } else {
                self.file(&entry)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn from_checkout(
    root: &Path,
    changed: impl FnMut(SourceKind, &Path),
) -> io::Result<u64> {
    let mut scan = Scan {
        root,
        changed,
        hasher: blake3::Hasher::new(),
        bytes: 0,
    };
    (scan.changed)(SourceKind::Directory, root);
    for file in FILES {
        scan.file(&root.join(file))?;
    }
    for directory in ROOTS {
        scan.directory(&root.join(directory))?;
    }
    let cargo_config = root.join(".cargo");
    match fs::symlink_metadata(&cargo_config) {
        Ok(_) => scan.directory(&cargo_config)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let hash = scan.hasher.finalize();
    let mut prefix = [0_u8; 8];
    for (target, source) in prefix.iter_mut().zip(hash.as_bytes().iter()) {
        *target = *source;
    }
    Ok(u64::from_be_bytes(prefix))
}

#[cfg(test)]
mod tests {
    use super::{FILES, ROOTS, from_checkout};
    use crate::testing;

    fn fingerprint_changes() -> std::io::Result<[u64; 4]> {
        let checkout = tempfile::tempdir()?;
        for file in FILES {
            testing::write(&checkout.path().join(file), b"original");
        }
        for directory in ROOTS {
            testing::write(
                &checkout.path().join(directory).join("Cargo.toml"),
                b"original",
            );
        }
        for directory in ["crates/domyjob", "crates/domyjob-core"] {
            testing::write(
                &checkout.path().join(directory).join("Cargo.toml"),
                b"original",
            );
        }
        let original = from_checkout(checkout.path(), |_kind, _path| {})?;
        let added = checkout.path().join("crates/domyjob/src/extra.rs");
        testing::write(&added, b"added");
        let with_addition = from_checkout(checkout.path(), |_kind, _path| {})?;
        testing::remove(&added);
        let restored = from_checkout(checkout.path(), |_kind, _path| {})?;
        testing::write(
            &checkout.path().join("crates/domyjob/Cargo.toml"),
            b"changed",
        );
        let with_change = from_checkout(checkout.path(), |_kind, _path| {})?;
        Ok([original, with_addition, restored, with_change])
    }

    #[test]
    fn source_addition_change_and_deletion_change_the_build() {
        let [original, with_addition, restored, with_change] =
            fingerprint_changes().expect("temporary checkout");
        assert_ne!(with_addition, original);
        assert_eq!(restored, original);
        assert_ne!(with_change, original);
    }
}
