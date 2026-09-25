#![no_main]

use std::sync::LazyLock;

use domyjob::dist::{ReleaseKey, Signatures, verify_manifest_with};

static PUBLIC: LazyLock<xtask::release::Public> =
    LazyLock::new(|| xtask::release::Secret::generate().unwrap().public());

libfuzzer_sys::fuzz_target!(|input: (&[u8], String, String)| {
    let (manifest, minisign, ml_dsa) = input;
    let key = ReleaseKey {
        minisign: &PUBLIC.minisign,
        ml_dsa: &PUBLIC.ml_dsa,
    };
    let signatures = Signatures { minisign, ml_dsa };
    assert!(verify_manifest_with(manifest, &signatures, &[key]).is_err());
});
