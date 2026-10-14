use alloc::string::String;
use alloc::vec::Vec;

use crate::domain::{Invalid, RelativePath};

pub const MAX_FILES: usize = 100_000;
pub const MAX_PATH_BYTES: usize = 16_777_216;
pub const MAX_TRACKED_BYTES: usize = MAX_PATH_BYTES + MAX_FILES;
const MAX_RECORD_BYTES: usize = 4096;
pub const GIT_CONTEXT_VARIABLES: [&str; 16] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

mod metadata;
mod read_length;
pub use metadata::metadata_component;
pub use read_length::ReadLength;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrackedError {
    #[error("the tracked paths exceed their byte, file, or path budget")]
    Capacity,
    #[error("a tracked path is empty")]
    Empty,
    #[error("the tracked path list is not NUL-terminated")]
    Unterminated,
    #[error("a tracked path is not UTF-8")]
    NotUtf8,
    #[error(transparent)]
    Invalid(#[from] Invalid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedPaths(Vec<RelativePath>);

impl TrackedPaths {
    #[must_use]
    pub fn paths(&self) -> &[RelativePath] {
        &self.0
    }
}

#[must_use]
pub fn metadata_path(path: &RelativePath) -> bool {
    metadata::metadata_path(path.as_str())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PathBudget {
    bytes: usize,
    records: usize,
    record_bytes: usize,
    path_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Byte,
    Path,
}

impl PathBudget {
    fn advance(self, byte: u8) -> Result<(Self, Boundary), TrackedError> {
        let bytes = self.bytes.checked_add(1).ok_or(TrackedError::Capacity)?;
        if bytes > MAX_TRACKED_BYTES || self.records >= MAX_FILES {
            return Err(TrackedError::Capacity);
        }
        if byte == 0 {
            if self.record_bytes == 0 {
                return Err(TrackedError::Empty);
            }
            let records = self.records.checked_add(1).ok_or(TrackedError::Capacity)?;
            return Ok((
                Self {
                    bytes,
                    records,
                    record_bytes: 0,
                    path_bytes: self.path_bytes,
                },
                Boundary::Path,
            ));
        }
        let record_bytes = self
            .record_bytes
            .checked_add(1)
            .ok_or(TrackedError::Capacity)?;
        let path_bytes = self
            .path_bytes
            .checked_add(1)
            .ok_or(TrackedError::Capacity)?;
        if record_bytes > MAX_RECORD_BYTES || path_bytes > MAX_PATH_BYTES {
            return Err(TrackedError::Capacity);
        }
        Ok((
            Self {
                bytes,
                records: self.records,
                record_bytes,
                path_bytes,
            },
            Boundary::Byte,
        ))
    }

    const fn finish(self) -> Result<(), TrackedError> {
        if self.record_bytes == 0 {
            Ok(())
        } else {
            Err(TrackedError::Unterminated)
        }
    }
}

pub fn tracked_paths(bytes: &[u8]) -> Result<TrackedPaths, TrackedError> {
    if bytes.len() > MAX_TRACKED_BYTES {
        return Err(TrackedError::Capacity);
    }
    let mut budget = PathBudget::default();
    let mut record = Vec::new();
    let mut paths = Vec::new();
    for &byte in bytes {
        let (next, boundary) = budget.advance(byte)?;
        budget = next;
        match boundary {
            Boundary::Byte => record.push(byte),
            Boundary::Path => {
                let text = String::from_utf8(core::mem::take(&mut record))
                    .map_err(|_encoding| TrackedError::NotUtf8)?;
                let path = RelativePath::try_from(text)?;
                if !metadata_path(&path) {
                    paths.push(path);
                }
            }
        }
    }
    budget.finish()?;
    Ok(TrackedPaths(paths))
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned as _;
    use alloc::vec;

    use super::{MAX_FILES, MAX_PATH_BYTES, TrackedError, metadata_component, tracked_paths};

    #[test]
    fn tracked_records_are_exact_portable_paths() {
        assert_eq!(tracked_paths(b"").unwrap().paths(), []);
        assert_eq!(
            tracked_paths(b"CLAUDE.md\0src/main.rs\0").unwrap().paths(),
            [
                crate::domain::RelativePath::try_from("CLAUDE.md".to_owned()).unwrap(),
                crate::domain::RelativePath::try_from("src/main.rs".to_owned()).unwrap(),
            ]
        );
        assert_eq!(tracked_paths(b"\0"), Err(TrackedError::Empty));
        assert_eq!(tracked_paths(b"missing"), Err(TrackedError::Unterminated));
        assert_eq!(tracked_paths(b"\xff\0"), Err(TrackedError::NotUtf8));
        for bytes in [
            b"../escape\0".as_slice(),
            b"/absolute\0",
            b"a\\b\0",
            b"a//b\0",
        ] {
            assert!(matches!(
                tracked_paths(bytes),
                Err(TrackedError::Invalid(_))
            ));
        }
    }

    #[test]
    fn tracked_records_hold_every_budget_boundary() {
        let mut largest = vec![b'a'; 4096];
        largest.push(0);
        assert_eq!(tracked_paths(&largest).unwrap().paths().len(), 1);
        largest.insert(0, b'a');
        assert_eq!(tracked_paths(&largest), Err(TrackedError::Capacity));
        let largest_count = b"a\0".repeat(MAX_FILES);
        assert_eq!(
            tracked_paths(&largest_count).unwrap().paths().len(),
            MAX_FILES
        );
        assert_eq!(
            tracked_paths(&b"a\0".repeat(MAX_FILES.saturating_add(1))),
            Err(TrackedError::Capacity)
        );
        assert_eq!(
            tracked_paths(&vec![
                b'a';
                MAX_PATH_BYTES
                    .saturating_add(MAX_FILES)
                    .saturating_add(1)
            ]),
            Err(TrackedError::Capacity)
        );
        let mut aggregate = vec![b'a'; super::MAX_RECORD_BYTES];
        aggregate.push(0);
        let mut aggregate = aggregate.repeat(MAX_PATH_BYTES / super::MAX_RECORD_BYTES);
        assert_eq!(
            tracked_paths(&aggregate).unwrap().paths().len(),
            MAX_PATH_BYTES / super::MAX_RECORD_BYTES
        );
        aggregate.extend_from_slice(b"b\0");
        assert_eq!(tracked_paths(&aggregate), Err(TrackedError::Capacity));
    }

    #[test]
    fn every_repository_metadata_component_is_excluded_at_any_depth() {
        for metadata in super::metadata::METADATA {
            assert!(metadata_component(metadata));
            for path in [
                alloc::format!("{metadata}/private\0"),
                alloc::format!("nested/{metadata}/private\0"),
                alloc::format!("nested/{metadata}\0"),
            ] {
                assert_eq!(tracked_paths(path.as_bytes()).unwrap().paths(), []);
            }
            let upper = metadata.to_ascii_uppercase();
            assert!(metadata_component(&upper));
            assert_eq!(
                tracked_paths(alloc::format!("nested/{upper}/private\0").as_bytes())
                    .unwrap()
                    .paths(),
                []
            );
        }
        assert_eq!(tracked_paths(b".gitignore\0").unwrap().paths().len(), 1);
        assert_eq!(tracked_paths(b"git/normal\0").unwrap().paths().len(), 1);
    }
}

#[cfg(kani)]
mod proofs {
    use super::{
        Boundary, MAX_FILES, MAX_PATH_BYTES, MAX_RECORD_BYTES, MAX_TRACKED_BYTES, PathBudget,
        TrackedError,
    };
    #[kani::proof]
    fn tracked_path_records_are_bounded() {
        let budget = PathBudget {
            bytes: kani::any(),
            records: kani::any(),
            record_bytes: kani::any(),
            path_bytes: kani::any(),
        };
        let byte: u8 = kani::any();
        kani::assume(budget.bytes <= MAX_TRACKED_BYTES);
        kani::assume(budget.records <= MAX_FILES);
        kani::assume(budget.record_bytes <= MAX_RECORD_BYTES);
        kani::assume(budget.path_bytes <= MAX_PATH_BYTES);
        let expected = budget.bytes < MAX_TRACKED_BYTES
            && budget.records < MAX_FILES
            && if byte == 0 {
                budget.record_bytes > 0
            } else {
                budget.record_bytes < MAX_RECORD_BYTES && budget.path_bytes < MAX_PATH_BYTES
            };
        let result = budget.advance(byte);
        assert_eq!(result.is_ok(), expected);
        if let Ok((next, boundary)) = result {
            assert!(next.bytes <= MAX_TRACKED_BYTES);
            assert!(next.records <= MAX_FILES);
            assert!(next.record_bytes <= MAX_RECORD_BYTES);
            assert!(next.path_bytes <= MAX_PATH_BYTES);
            match boundary {
                Boundary::Path => {
                    assert_eq!(next.record_bytes, 0);
                    assert_eq!(next.path_bytes, budget.path_bytes);
                    assert!(next.records > budget.records);
                }
                Boundary::Byte => {
                    assert_eq!(next.records, budget.records);
                    assert!(next.record_bytes > budget.record_bytes);
                    assert!(next.path_bytes > budget.path_bytes);
                }
            }
        }
        assert_eq!(budget.finish().is_ok(), budget.record_bytes == 0);
        kani::cover!(result.is_ok() && byte != 0);
        kani::cover!(result == Err(TrackedError::Empty));
        kani::cover!(budget.finish() == Err(TrackedError::Unterminated));
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_is_excluded() {
        const PATHS: [&str; 16] = [
            ".git/x",
            ".jj/x",
            ".hg/x",
            ".svn/x",
            ".pijul/x",
            "_darcs/x",
            ".bzr/x",
            "CVS/x",
            "x/.git/y",
            "x/.jj/y",
            "x/.hg/y",
            "x/.svn/y",
            "x/.pijul/y",
            "x/_darcs/y",
            "x/.bzr/y",
            "x/CVS/y",
        ];
        let selection: usize = kani::any();
        kani::assume(selection < PATHS.len());
        let path = PATHS.get(selection).unwrap();
        assert!(super::metadata::metadata_path(path));
        let metadata: bool = kani::any();
        let path = if metadata { "x/.git/y" } else { "x/.gitignore" };
        assert_eq!(super::metadata::metadata_path(path), metadata);
        kani::cover!(metadata);
        kani::cover!(!metadata);
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_git_casing_is_excluded() {
        metadata_casing(b".git");
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_jj_casing_is_excluded() {
        metadata_casing(b".jj");
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_hg_casing_is_excluded() {
        metadata_casing(b".hg");
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_svn_casing_is_excluded() {
        metadata_casing(b".svn");
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_pijul_casing_is_excluded() {
        metadata_casing(b".pijul");
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_darcs_casing_is_excluded() {
        metadata_casing(b"_darcs");
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_bzr_casing_is_excluded() {
        metadata_casing(b".bzr");
    }

    #[kani::proof]
    #[kani::unwind(16)]
    fn repository_metadata_cvs_casing_is_excluded() {
        metadata_casing(b"CVS");
    }

    const METADATA_CASE_CHOICES: usize = 6;

    fn metadata_casing<const N: usize>(name: &[u8; N]) {
        let uppercase: [bool; METADATA_CASE_CHOICES] = kani::any();
        check_metadata_case(name, &uppercase);
        kani::cover!(uppercase.iter().all(|upper| *upper));
        kani::cover!(uppercase.iter().all(|upper| !*upper));
    }

    fn check_metadata_case<const N: usize>(
        name: &[u8; N],
        uppercase: &[bool; METADATA_CASE_CHOICES],
    ) {
        let mut bytes = *name;
        for (byte, upper) in bytes.iter_mut().zip(uppercase) {
            if *upper {
                byte.make_ascii_uppercase();
            } else {
                byte.make_ascii_lowercase();
            }
        }
        let text = core::str::from_utf8(&bytes).unwrap();
        assert!(super::metadata_component(text));
    }
}
