#![no_main]

use domyjob_core::cache_key::{CacheAddress, CacheKey};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    for candidate in [bytes, bytes.strip_suffix(b"\n").unwrap_or(bytes)] {
        for kind in [
            CacheAddress::Route,
            CacheAddress::Expanded,
            CacheAddress::Legacy,
        ] {
            if kind.admits(candidate) {
                assert_eq!(candidate.len(), kind.width());
                let letters = if kind == CacheAddress::Legacy {
                    b'f'
                } else {
                    b'v'
                };
                assert!(
                    candidate.iter().all(|byte| byte.is_ascii_digit()
                        || (byte.is_ascii_lowercase() && *byte <= letters))
                );
                if kind == CacheAddress::Expanded {
                    assert!(matches!(candidate.last(), Some(b'0' | b'g')));
                }
            }
        }
    }
    if let Ok(key) =
        domyjob_core::ingress::json::<CacheKey>(bytes, domyjob_core::wire::MAX_CONTROL_BYTES)
    {
        let saved = serde_json::to_vec(&key).unwrap();
        assert_eq!(
            domyjob_core::ingress::json::<CacheKey>(&saved, domyjob_core::wire::MAX_CONTROL_BYTES)
                .unwrap(),
            key
        );
    }
});
