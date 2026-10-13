use crate::wire::MAX_SNAPSHOT_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLength(u64);

impl ReadLength {
    #[must_use]
    pub const fn new(length: u64) -> Option<Self> {
        if length <= MAX_SNAPSHOT_BYTES {
            Some(Self(length))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn limit(self) -> u64 {
        self.0.saturating_add(1)
    }

    #[must_use]
    pub const fn matches(self, actual: u64) -> bool {
        self.0 == actual
    }
}

#[cfg(kani)]
mod proofs {
    use super::{MAX_SNAPSHOT_BYTES, ReadLength};

    #[kani::proof]
    fn source_reads_are_bounded_and_exact() {
        let expected: u64 = kani::any();
        let actual: u64 = kani::any();
        let length = ReadLength::new(expected);
        assert_eq!(length.is_some(), expected <= MAX_SNAPSHOT_BYTES);
        if let Some(length) = length {
            assert_eq!(length.limit(), expected + 1);
            assert!(length.limit() <= MAX_SNAPSHOT_BYTES + 1);
            assert_eq!(length.matches(actual), actual == expected);
        }
        kani::cover!(length.is_some() && actual == expected);
        kani::cover!(length.is_some() && actual != expected);
        kani::cover!(length.is_none());
    }

    #[kani::proof]
    fn rejects_a_changed_read_length() {
        kani::assert(
            ReadLength::new(1).unwrap().matches(2),
            "NEGATIVE_SOURCE_READ_CONTROL",
        );
    }
}
