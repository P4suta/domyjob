use domyjob::dist::{ReleaseKey, Signatures, verify_manifest_with};
use xtask::release::Secret;

const MANIFEST: &[u8] = br#"{"version":"1.2.3","targets":{}}"#;

fn signatures(secret: &Secret, bytes: &[u8]) -> Result<Signatures, xtask::release::ReleaseError> {
    let signed = secret.sign(bytes, "domyjob 1.2.3")?;
    Ok(Signatures {
        minisign: signed.minisig,
        ml_dsa: signed.ml_dsa,
    })
}

#[test]
fn a_release_verifies_only_when_both_halves_hold() {
    let secret = Secret::generate().unwrap();
    let public = secret.public();
    let keys = [ReleaseKey {
        minisign: &public.minisign,
        ml_dsa: &public.ml_dsa,
    }];
    let good = signatures(&secret, MANIFEST).unwrap();
    let verified = verify_manifest_with(MANIFEST, &good, &keys).unwrap();
    assert_eq!(verified.get().version, "1.2.3");

    let tampered = br#"{"version":"9.9.9","targets":{}}"#;
    verify_manifest_with(tampered, &good, &keys).unwrap_err();

    let stranger = Secret::generate().unwrap();
    let theirs = signatures(&stranger, MANIFEST).unwrap();
    let mixed_classical = Signatures {
        minisign: theirs.minisign.clone(),
        ml_dsa: good.ml_dsa.clone(),
    };
    verify_manifest_with(MANIFEST, &mixed_classical, &keys).unwrap_err();
    let mixed_quantum = Signatures {
        minisign: good.minisign.clone(),
        ml_dsa: theirs.ml_dsa,
    };
    verify_manifest_with(MANIFEST, &mixed_quantum, &keys).unwrap_err();
    let classical_only = Signatures {
        minisign: good.minisign,
        ml_dsa: String::new(),
    };
    verify_manifest_with(MANIFEST, &classical_only, &keys).unwrap_err();
}

#[test]
fn secrets_round_trip_through_their_file_format() {
    let secret = Secret::generate().unwrap();
    let path = std::path::Path::new("release.secret");
    let again = Secret::decode(&secret.encode(), path).unwrap();
    assert_eq!(again.public(), secret.public());
    Secret::decode("not a secret", path).unwrap_err();
}
