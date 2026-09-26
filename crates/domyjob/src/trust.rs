use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::authz::Capability;
use crate::clock::Timestamp;
use crate::domain::{Invalid, MachineName};
use crate::paths::Dirs;
use crate::secret::Secret;
use crate::state_file::{self, StateError};

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Key(#[from] crate::keystore::KeyError),
    #[error("the random source failed: {0}")]
    Random(getrandom::Error),
    #[error("nothing paired matches {0}")]
    Unknown(String),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
    #[error(
        "this machine's identity key could not be found, yet it is paired with other machines, so a new key would lock them out; if the key lives in a locked keychain, unlock it and try again, or remove {0} to start over deliberately"
    )]
    Vanished(PathBuf),
    #[error(transparent)]
    Invalid(#[from] Invalid),
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

#[must_use]
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    match data_encoding::HEXLOWER_PERMISSIVE.decode(text.as_bytes()) {
        Ok(bytes) => Some(bytes),
        Err(_not_hex) => None,
    }
}

pub(crate) fn unhex32(text: &str) -> Option<[u8; 32]> {
    match <[u8; 32]>::try_from(unhex(text)?) {
        Ok(bytes) => Some(bytes),
        Err(_wrong_length) => None,
    }
}

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct PublicKey([u8; 32]);

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint())
    }
}

impl TryFrom<String> for PublicKey {
    type Error = Invalid;

    fn try_from(text: String) -> Result<Self, Invalid> {
        unhex32(&text).map(Self).ok_or(Invalid::Key(text))
    }
}

impl From<PublicKey> for String {
    fn from(key: PublicKey) -> Self {
        hex(&key.0)
    }
}

impl PublicKey {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, Invalid> {
        match <[u8; 32]>::try_from(bytes) {
            Ok(array) => Ok(Self(array)),
            Err(_wrong_length) => Err(Invalid::Key(hex(bytes))),
        }
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        let digest = blake3::derive_key("domyjob 2026 key fingerprint v1", &self.0);
        let text = hex(digest.get(..16).unwrap_or(&[]));
        let groups: Vec<String> = text
            .as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        groups.join("-")
    }
}

#[derive(Debug)]
pub struct Identity {
    secret: Secret<[u8; 32]>,
    public: PublicKey,
}

fn derive_public(secret: &[u8; 32]) -> PublicKey {
    let key = x25519_dalek::StaticSecret::from(*secret);
    PublicKey(x25519_dalek::PublicKey::from(&key).to_bytes())
}

impl Identity {
    pub fn load_or_create(dirs: &Dirs) -> Result<Self, TrustError> {
        if let Some(found) = dirs.keys.load(&dirs.state, "identity")? {
            return Ok(Self {
                public: derive_public(found.expose()),
                secret: found,
            });
        }
        let lock = crate::lock::OsLock::exclusive(&dirs.state.join("identity.lock"))?;
        let secret = match dirs.keys.load(&dirs.state, "identity")? {
            Some(found) => found,
            None => {
                let trust = Trust::load(dirs)?;
                if !trust.servers.is_empty() || !trust.grants.is_empty() {
                    return Err(TrustError::Vanished(Trust::path(dirs)));
                }
                let mut fresh = [0u8; 32];
                getrandom::fill(&mut fresh).map_err(TrustError::Random)?;
                let fresh = Secret::new(fresh);
                dirs.keys.store(&dirs.state, "identity", fresh.expose())?;
                fresh
            }
        };
        lock.release()?;
        Ok(Self {
            public: derive_public(secret.expose()),
            secret,
        })
    }

    #[must_use]
    pub const fn protection(dirs: &Dirs) -> crate::keystore::Protection {
        dirs.keys.protection()
    }

    #[must_use]
    pub const fn secret(&self) -> &[u8; 32] {
        self.secret.expose()
    }

    #[must_use]
    pub const fn public(&self) -> &PublicKey {
        &self.public
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub public_key: PublicKey,
    pub address: String,
    pub paired_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub label: MachineName,
    pub public_key: PublicKey,
    pub capabilities: BTreeSet<Capability>,
    pub granted_at: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    pub servers: BTreeMap<MachineName, Server>,
    pub grants: Vec<Grant>,
}

impl Trust {
    fn path(dirs: &Dirs) -> PathBuf {
        dirs.state.join("trust.json")
    }

    pub fn load(dirs: &Dirs) -> Result<Self, TrustError> {
        Ok(state_file::read_json(&Self::path(dirs))?.unwrap_or_default())
    }

    pub fn update<R>(dirs: &Dirs, change: impl FnOnce(&mut Self) -> R) -> Result<R, TrustError> {
        Ok(state_file::update_json(
            &Self::path(dirs),
            Self::default,
            change,
        )?)
    }

    #[must_use]
    pub fn grant_for(&self, key: &PublicKey) -> Option<&Grant> {
        self.grants.iter().find(|grant| &grant.public_key == key)
    }
}

impl crate::ingress::Ingress for Trust {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &std::path::Path) -> Dirs {
        Dirs::for_test(root)
    }

    fn observed(key: PublicKey) -> Grant {
        Grant {
            label: "mac".parse().unwrap(),
            public_key: key,
            capabilities: BTreeSet::from([Capability::Observe]),
            granted_at: Timestamp::at_millis(1),
        }
    }

    fn add_observer(dirs: &Dirs, key: PublicKey) -> Result<(), TrustError> {
        Trust::update(dirs, |trust| trust.grants.push(observed(key)))
    }

    #[test]
    fn identities_persist_and_derive_their_public_key() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(&dirs(tmp.path())).unwrap();
        let again = Identity::load_or_create(&dirs(tmp.path())).unwrap();
        assert_eq!(first.public(), again.public());
        assert!(format!("{first:?}").contains("redacted"));
        let text: String = (*first.public()).into();
        assert_eq!(PublicKey::try_from(text).unwrap(), *first.public());
        PublicKey::try_from("zz".to_owned()).unwrap_err();
        assert_eq!(first.public().fingerprint().len(), 39);
    }

    #[test]
    fn a_missing_key_is_not_silently_replaced_while_pairings_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dirs(tmp.path());
        let key = *Identity::load_or_create(&d).unwrap().public();
        add_observer(&d, key).unwrap();
        state_file::remove_file(&d.state.join("identity.json")).unwrap();
        assert!(matches!(
            Identity::load_or_create(&d),
            Err(TrustError::Vanished(_))
        ));
        Trust::update(&d, |all| all.grants.clear()).unwrap();
        assert_ne!(*Identity::load_or_create(&d).unwrap().public(), key);
    }

    #[test]
    fn grants_last_until_revoked_and_are_found_by_key() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dirs(tmp.path());
        let key = *Identity::load_or_create(&d).unwrap().public();
        add_observer(&d, key).unwrap();
        let trust = Trust::load(&d).unwrap();
        assert!(trust.grant_for(&key).is_some());
        Trust::update(&d, |all| all.grants.clear()).unwrap();
        assert!(Trust::load(&d).unwrap().grant_for(&key).is_none());
    }

    #[test]
    fn a_crash_while_changing_trust_leaves_the_old_or_new_whole_file() {
        let key = PublicKey::from_slice(&[7; 32]).unwrap();
        let steps = {
            let tmp = tempfile::tempdir().unwrap();
            let d = dirs(tmp.path());
            Trust::update(&d, |_| {}).unwrap();
            let crashing = crate::faults::crash_after(tmp.path(), None);
            add_observer(&d, key).unwrap();
            crashing.steps()
        };
        for step in 0..steps {
            let tmp = tempfile::tempdir().unwrap();
            let d = dirs(tmp.path());
            Trust::update(&d, |_| {}).unwrap();
            {
                let _crashing = crate::faults::crash_after(tmp.path(), Some(step));
                match add_observer(&d, key) {
                    Ok(()) | Err(_) => {}
                }
            }
            let trust = Trust::load(&d).unwrap();
            assert!(matches!(trust.grants.len(), 0 | 1));
        }
    }

    #[test]
    fn damaged_or_unwritable_state_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dirs(tmp.path());
        state_file::write_bytes(&d.state.join("trust.json"), b"not trust").unwrap();
        Trust::load(&d).unwrap_err();
        Trust::update(&d, |_| ()).unwrap_err();
        state_file::write_bytes(&d.state.join("identity.json"), b"not a key").unwrap();
        Identity::load_or_create(&d).map(drop).unwrap_err();
    }

    #[test]
    fn an_identity_that_cannot_be_saved_is_an_error() {
        if !crate::platform::MODES {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let d = dirs(tmp.path());
        let _read_only = crate::platform::ReadOnly::make(&d.state).unwrap();
        Identity::load_or_create(&d).map(drop).unwrap_err();
    }

    proptest::proptest! {
        #[test]
        fn hex_round_trips(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..80)) {
            proptest::prop_assert_eq!(unhex(&hex(&bytes)), Some(bytes.clone()));
            proptest::prop_assert_eq!(unhex32(&hex(&bytes)).is_some(), bytes.len() == 32);
        }

        #[test]
        fn garbage_is_never_a_key(text in "[^0-9a-fA-F]{1,70}") {
            proptest::prop_assert!(unhex(&text).is_none());
        }

        #[test]
        fn an_odd_number_of_digits_is_never_bytes(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..40)) {
            let mut text = hex(&bytes);
            text.push('a');
            proptest::prop_assert!(unhex(&text).is_none());
        }
    }
}
