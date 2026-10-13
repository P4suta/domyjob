use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use cap_std::fs::Dir;
use domyjob_core::source_paths::ReadLength;

use crate::file_kind;
use crate::source_archive::source_file;

const MAX_SOURCE_BYTES: u64 = 67_108_864;
const READ_CHUNK_BYTES: usize = 16_384;
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
    directory: Dir,
    changed: F,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl<F: FnMut(SourceKind, &Path)> Scan<'_, F> {
    fn file(&mut self, path: &Path) -> io::Result<()> {
        (self.changed)(SourceKind::File, path);
        let relative = path
            .strip_prefix(self.root)
            .map_err(|_prefix| io::Error::other("source is outside the checkout"))?;
        let mut file = source_file::open(&self.directory, relative)?;
        let metadata = file.metadata()?;
        self.bytes = self
            .bytes
            .checked_add(metadata.len())
            .ok_or_else(|| io::Error::other("source size overflowed"))?;
        if self.bytes > MAX_SOURCE_BYTES {
            return Err(io::Error::other("source files exceed 64 MiB"));
        }
        for part in relative.components() {
            let text = part
                .as_os_str()
                .to_str()
                .ok_or_else(|| io::Error::other("a source path is not UTF-8"))?;
            self.hasher.update(text.as_bytes());
            self.hasher.update(&[0]);
        }
        self.hasher.update(&metadata.len().to_be_bytes());
        let length = ReadLength::new(metadata.len())
            .ok_or_else(|| io::Error::other("source files exceed 64 MiB"))?;
        hash_content(&mut self.hasher, &mut file, length)?;
        Ok(())
    }

    fn directory(&mut self, path: &Path) -> io::Result<()> {
        (self.changed)(SourceKind::Directory, path);
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || file_kind::reparse_point(&metadata) {
            return Err(io::Error::other("a source path is not a directory"));
        }
        let relative = path
            .strip_prefix(self.root)
            .map_err(|_prefix| io::Error::other("source is outside the checkout"))?;
        let captured = source_file::directory(&self.directory, relative)?;
        let mut entries = captured
            .entries()?
            .map(|entry| entry.map(|entry| path.join(entry.file_name())))
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

fn hash_content(
    hasher: &mut blake3::Hasher,
    file: impl Read,
    length: ReadLength,
) -> io::Result<()> {
    let mut file = file.take(length.limit());
    let mut buffer = [0_u8; READ_CHUNK_BYTES];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let chunk = buffer
            .get(..count)
            .ok_or_else(|| io::Error::other("source read exceeded the buffer"))?;
        hasher.update(chunk);
    }
    if !length.matches(length.limit().saturating_sub(file.limit())) {
        return Err(io::Error::other("a source file changed while reading"));
    }
    Ok(())
}

pub(crate) fn from_checkout(
    root: &Path,
    changed: impl FnMut(SourceKind, &Path),
) -> io::Result<u64> {
    let mut scan = Scan {
        root,
        directory: Dir::open_ambient_dir(root, cap_std::ambient_authority())?,
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
    use std::io::{self, Cursor};

    use super::{FILES, ROOTS, from_checkout};
    use crate::testing;
    use domyjob_core::source_paths::ReadLength;

    fn fingerprint_changes() -> io::Result<[u64; 4]> {
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

    #[test]
    fn fingerprint_reads_refuse_growth_and_truncation_with_one_extra_byte_at_most() {
        const EXPECTED_BYTES: u64 = 3;
        let length = ReadLength::new(EXPECTED_BYTES).unwrap();
        for bytes in [b"abc".as_slice(), b"ab", b"abcde"] {
            let mut content = Cursor::new(bytes);
            let result = super::hash_content(&mut blake3::Hasher::new(), &mut content, length);
            assert_eq!(result.is_ok(), bytes == b"abc");
            assert!(content.position() <= length.limit());
            assert_eq!(
                content.position(),
                u64::try_from(bytes.len()).unwrap().min(length.limit())
            );
        }
    }

    #[test]
    fn fingerprint_reads_preserve_the_original_io_error() {
        struct Broken;
        impl io::Read for Broken {
            fn read(&mut self, _bytes: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
        }
        let error = super::hash_content(
            &mut blake3::Hasher::new(),
            Broken,
            ReadLength::new(0).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
