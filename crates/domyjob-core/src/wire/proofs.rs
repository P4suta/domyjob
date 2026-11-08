use alloc::string::String;

use super::{MAX_SNAPSHOT_BYTES, Snapshot};
use crate::cache_key::CacheKey;

const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[kani::proof]
#[kani::unwind(65)]
fn cache_normalization_preserves_snapshot_identity() {
    let bytes: u64 = kani::any();
    kani::assume(bytes <= MAX_SNAPSHOT_BYTES);
    let cached: bool = kani::any();
    let cache = CacheKey::new(kani::any(), kani::any());
    let snapshot = Snapshot {
        bytes,
        digest: String::from(DIGEST),
        cache: cached.then_some(cache),
    };
    let pointer = snapshot.digest().as_ptr();
    let length = snapshot.digest().len();
    let normalized = snapshot.without_cache();
    assert_eq!(normalized.bytes(), bytes);
    assert_eq!(normalized.digest().as_ptr(), pointer);
    assert_eq!(normalized.digest().len(), length);
    assert!(normalized.cache().is_none());
    let repeated = normalized.without_cache();
    assert_eq!(repeated.bytes(), bytes);
    assert_eq!(repeated.digest().as_ptr(), pointer);
    assert_eq!(repeated.digest().len(), length);
    assert!(repeated.cache().is_none());
    kani::cover!(cached);
    kani::cover!(!cached);
}

#[kani::proof]
#[kani::unwind(65)]
fn rejects_cache_identity_in_submission_history() {
    let snapshot = Snapshot {
        bytes: 0,
        digest: String::from(DIGEST),
        cache: Some(CacheKey::new([1; 32], [2; 32])),
    };
    kani::assert(
        snapshot.without_cache().cache().is_some(),
        "NEGATIVE_CACHE_NORMALIZATION_CONTROL",
    );
}
