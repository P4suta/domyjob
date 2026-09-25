use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::secret::Secret;
use crate::state_file::{self, StateError};

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error("{0} does not hold 32 bytes of hex")]
    Malformed(PathBuf),
    #[error(
        "the macOS keychain refused to {doing} the {name} key ({detail}); if this is an ssh session, unlock it with `security unlock-keychain` or run the command from your login session"
    )]
    Keychain {
        doing: &'static str,
        name: &'static str,
        detail: String,
    },
    #[error(
        "Windows could not {doing} the {name} key with the Data Protection API ({detail}); an ssh session signed in with a key has no password to unlock it, so run the command from your Windows session"
    )]
    Dpapi {
        doing: &'static str,
        name: &'static str,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStore {
    Platform,
    OwnerOnlyFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    MacKeychain,
    WindowsDpapi,
    OwnerOnlyFile,
}

impl Protection {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::MacKeychain => "the macOS keychain",
            Self::WindowsDpapi => "the Windows Data Protection API, bound to your sign-in",
            Self::OwnerOnlyFile => "a file only you can read",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredKey {
    secret: String,
}

impl crate::ingress::Ingress for StoredKey {}

fn file(state: &Path, name: &str) -> PathBuf {
    state.join(format!("{name}.json"))
}

fn read_file(path: &Path) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
    match state_file::read_json::<StoredKey>(path)? {
        Some(stored) => crate::trust::unhex32(&stored.secret)
            .map(|bytes| Some(Secret::new(bytes)))
            .ok_or_else(|| KeyError::Malformed(path.to_path_buf())),
        None => Ok(None),
    }
}

fn write_file(path: &Path, secret: &[u8; 32]) -> Result<(), KeyError> {
    Ok(state_file::write_json(
        path,
        &StoredKey {
            secret: crate::trust::hex(secret),
        },
    )?)
}

impl KeyStore {
    #[must_use]
    pub const fn protection(self) -> Protection {
        match self {
            Self::Platform => platform::PROTECTION,
            Self::OwnerOnlyFile => Protection::OwnerOnlyFile,
        }
    }

    pub fn forget(self, state: &Path, name: &'static str) -> Result<(), KeyError> {
        state_file::remove_file(&file(state, name))?;
        match self.protection() {
            Protection::OwnerOnlyFile => Ok(()),
            Protection::MacKeychain | Protection::WindowsDpapi => platform::forget(state, name),
        }
    }

    pub fn load(
        self,
        state: &Path,
        name: &'static str,
    ) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
        match self.protection() {
            Protection::OwnerOnlyFile => read_file(&file(state, name)),
            Protection::MacKeychain | Protection::WindowsDpapi => migrate(&Os, state, name),
        }
    }

    pub fn store(
        self,
        state: &Path,
        name: &'static str,
        secret: &[u8; 32],
    ) -> Result<(), KeyError> {
        match self.protection() {
            Protection::OwnerOnlyFile => write_file(&file(state, name), secret),
            Protection::MacKeychain | Protection::WindowsDpapi => Os.store(state, name, secret),
        }
    }
}

trait Vault {
    fn load(&self, state: &Path, name: &'static str) -> Result<Option<Secret<[u8; 32]>>, KeyError>;

    fn store(&self, state: &Path, name: &'static str, secret: &[u8; 32]) -> Result<(), KeyError>;
}

struct Os;

impl Vault for Os {
    fn load(&self, state: &Path, name: &'static str) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
        platform::load(state, name)
    }

    fn store(&self, state: &Path, name: &'static str, secret: &[u8; 32]) -> Result<(), KeyError> {
        platform::store(state, name, secret)
    }
}

fn migrate(
    vault: &impl Vault,
    state: &Path,
    name: &'static str,
) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
    if let Some(found) = vault.load(state, name)? {
        return Ok(Some(found));
    }
    let legacy = file(state, name);
    let Some(found) = read_file(&legacy)? else {
        return Ok(None);
    };
    vault.store(state, name, found.expose())?;
    state_file::remove_file(&legacy)?;
    Ok(Some(found))
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::Path;

    use super::{KeyError, Protection};
    use crate::secret::Secret;

    pub(super) const PROTECTION: Protection = Protection::MacKeychain;
    const SERVICE: &str = "domyjob";
    const NOT_FOUND: i32 = -25_300;

    fn account(state: &Path, name: &str) -> String {
        let place = blake3::hash(state.as_os_str().as_encoded_bytes());
        let tag = crate::trust::hex(place.as_bytes().get(..8).unwrap_or(&[]));
        format!("{name}.{tag}")
    }

    pub(super) fn load(
        state: &Path,
        name: &'static str,
    ) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
        use security_framework::passwords::get_generic_password;
        match get_generic_password(SERVICE, &account(state, name)) {
            Ok(bytes) => {
                let secret = Secret::new(bytes);
                <[u8; 32]>::try_from(secret.expose().as_slice())
                    .map(|key| Some(Secret::new(key)))
                    .map_err(|_wrong_length| KeyError::Keychain {
                        doing: "read",
                        name,
                        detail: "the stored key has the wrong length".to_owned(),
                    })
            }
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(error) => Err(KeyError::Keychain {
                doing: "read",
                name,
                detail: error.to_string(),
            }),
        }
    }

    pub(super) fn store(
        state: &Path,
        name: &'static str,
        secret: &[u8; 32],
    ) -> Result<(), KeyError> {
        security_framework::passwords::set_generic_password(SERVICE, &account(state, name), secret)
            .map_err(|error| KeyError::Keychain {
                doing: "store",
                name,
                detail: error.to_string(),
            })
    }

    pub(super) fn forget(state: &Path, name: &'static str) -> Result<(), KeyError> {
        match security_framework::passwords::delete_generic_password(SERVICE, &account(state, name))
        {
            Ok(()) => Ok(()),
            Err(error) if error.code() == NOT_FOUND => Ok(()),
            Err(error) => Err(KeyError::Keychain {
                doing: "remove",
                name,
                detail: error.to_string(),
            }),
        }
    }
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "the Data Protection API is only reachable through its C interface"
)]
mod platform {
    use std::path::Path;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    use super::{KeyError, Protection};
    use crate::secret::Secret;

    pub(super) const PROTECTION: Protection = Protection::WindowsDpapi;
    const ENTROPY: &[u8] = b"domyjob 2026 key at rest v1";

    fn sealed(state: &Path, name: &str) -> std::path::PathBuf {
        state.join(format!("{name}.dpapi"))
    }

    fn blob(bytes: &[u8]) -> std::io::Result<CRYPT_INTEGER_BLOB> {
        Ok(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len()).map_err(std::io::Error::other)?,
            pbData: bytes.as_ptr().cast_mut(),
        })
    }

    fn take(out: CRYPT_INTEGER_BLOB) -> Secret<Vec<u8>> {
        let len = match usize::try_from(out.cbData) {
            Ok(len) => len,
            Err(_huge) => 0,
        };
        let copied = if out.pbData.is_null() || len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(out.pbData, len) }.to_vec()
        };
        if !out.pbData.is_null() {
            unsafe {
                std::ptr::write_bytes(out.pbData, 0, len);
                LocalFree(out.pbData.cast());
            }
        }
        Secret::new(copied)
    }

    fn transform(input: &[u8], protect: bool) -> std::io::Result<Secret<Vec<u8>>> {
        let data = blob(input)?;
        let entropy = blob(ENTROPY)?;
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &raw const data,
                    std::ptr::null(),
                    &raw const entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &raw mut out,
                )
            } else {
                CryptUnprotectData(
                    &raw const data,
                    std::ptr::null_mut(),
                    &raw const entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &raw mut out,
                )
            }
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(take(out))
    }

    pub(super) fn load(
        state: &Path,
        name: &'static str,
    ) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
        let Some(sealed_bytes) = crate::state_file::read_bytes(&sealed(state, name))? else {
            return Ok(None);
        };
        let failed = |detail: String| KeyError::Dpapi {
            doing: "unseal",
            name,
            detail,
        };
        let opened = transform(&sealed_bytes, false).map_err(|e| failed(e.to_string()))?;
        <[u8; 32]>::try_from(opened.expose().as_slice())
            .map(|key| Some(Secret::new(key)))
            .map_err(|_wrong_length| failed("the sealed key has the wrong length".to_owned()))
    }

    pub(super) fn store(
        state: &Path,
        name: &'static str,
        secret: &[u8; 32],
    ) -> Result<(), KeyError> {
        let sealed_bytes = transform(secret, true).map_err(|error| KeyError::Dpapi {
            doing: "seal",
            name,
            detail: error.to_string(),
        })?;
        Ok(crate::state_file::write_bytes(
            &sealed(state, name),
            sealed_bytes.expose(),
        )?)
    }

    pub(super) fn forget(state: &Path, name: &'static str) -> Result<(), KeyError> {
        Ok(crate::state_file::remove_file(&sealed(state, name))?)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use std::path::Path;

    use super::{KeyError, Protection};
    use crate::secret::Secret;

    pub(super) const PROTECTION: Protection = Protection::OwnerOnlyFile;

    #[expect(
        clippy::unnecessary_wraps,
        clippy::missing_const_for_fn,
        reason = "the same signature as the keychain and DPAPI stores, which can fail"
    )]
    pub(super) fn load(
        _state: &Path,
        _name: &'static str,
    ) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
        Ok(None)
    }

    #[expect(
        clippy::unnecessary_wraps,
        clippy::missing_const_for_fn,
        reason = "the same signature as the keychain and DPAPI stores, which can fail"
    )]
    pub(super) fn store(
        _state: &Path,
        _name: &'static str,
        _secret: &[u8; 32],
    ) -> Result<(), KeyError> {
        Ok(())
    }

    #[expect(
        clippy::unnecessary_wraps,
        clippy::missing_const_for_fn,
        reason = "the same signature as the keychain and DPAPI stores, which can fail"
    )]
    pub(super) fn forget(_state: &Path, _name: &'static str) -> Result<(), KeyError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_only_files_round_trip_and_refuse_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let store = KeyStore::OwnerOnlyFile;
        assert!(store.load(&state, "identity").unwrap().is_none());
        store.store(&state, "identity", &[7u8; 32]).unwrap();
        assert_eq!(
            store.load(&state, "identity").unwrap().unwrap().expose(),
            &[7u8; 32]
        );
        state_file::write_bytes(&state.join("broken.json"), br#"{"secret":"zz"}"#).unwrap();
        assert!(matches!(
            store.load(&state, "broken"),
            Err(KeyError::Malformed(_))
        ));
        state_file::write_bytes(&state.join("mangled.json"), b"not json").unwrap();
        store.load(&state, "mangled").map(drop).unwrap_err();
    }

    #[cfg(unix)]
    #[test]
    fn a_key_that_cannot_be_written_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let _read_only = state_file::ReadOnly::make(&state).unwrap();
        KeyStore::OwnerOnlyFile
            .store(&state, "identity", &[7u8; 32])
            .unwrap_err();
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Refuses {
        Nothing,
        Loading,
        Storing,
    }

    struct Fake {
        held: std::cell::Cell<Option<[u8; 32]>>,
        refuses: Refuses,
    }

    impl Fake {
        fn holding(held: Option<[u8; 32]>, refuses: Refuses) -> Self {
            Self {
                held: std::cell::Cell::new(held),
                refuses,
            }
        }

        fn held(&self) -> Option<[u8; 32]> {
            self.held.get()
        }

        fn refusal(&self, when: Refuses) -> Result<(), KeyError> {
            if self.refuses == when {
                Err(KeyError::Malformed(PathBuf::from("the vault")))
            } else {
                Ok(())
            }
        }
    }

    impl Vault for Fake {
        fn load(
            &self,
            _state: &Path,
            _name: &'static str,
        ) -> Result<Option<Secret<[u8; 32]>>, KeyError> {
            self.refusal(Refuses::Loading)?;
            Ok(self.held().map(Secret::new))
        }

        fn store(
            &self,
            _state: &Path,
            _name: &'static str,
            secret: &[u8; 32],
        ) -> Result<(), KeyError> {
            self.refusal(Refuses::Storing)?;
            self.held.set(Some(*secret));
            Ok(())
        }
    }

    #[test]
    fn a_vault_key_wins_and_a_legacy_file_moves_into_the_vault_once() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let legacy = file(&state, "identity");
        write_file(&legacy, &[1u8; 32]).unwrap();

        let holding = Fake::holding(Some([2u8; 32]), Refuses::Nothing);
        let found = migrate(&holding, &state, "identity").unwrap().unwrap();
        assert_eq!(found.expose(), &[2u8; 32]);
        assert!(state_file::read_bytes(&legacy).unwrap().is_some());

        let empty = Fake::holding(None, Refuses::Nothing);
        let moved = migrate(&empty, &state, "identity").unwrap().unwrap();
        assert_eq!(moved.expose(), &[1u8; 32]);
        assert_eq!(empty.held(), Some([1u8; 32]));
        assert!(state_file::read_bytes(&legacy).unwrap().is_none());
        assert!(
            migrate(&Fake::holding(None, Refuses::Nothing), &state, "identity")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_vault_or_legacy_file_that_fails_keeps_the_legacy_key() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let legacy = file(&state, "identity");
        write_file(&legacy, &[1u8; 32]).unwrap();
        for refuses in [Refuses::Loading, Refuses::Storing] {
            migrate(&Fake::holding(None, refuses), &state, "identity")
                .map(drop)
                .unwrap_err();
            assert!(state_file::read_bytes(&legacy).unwrap().is_some());
        }

        state_file::write_bytes(&legacy, b"not json").unwrap();
        let empty = Fake::holding(None, Refuses::Nothing);
        migrate(&empty, &state, "identity").map(drop).unwrap_err();
        assert_eq!(empty.held(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_file_that_cannot_be_removed_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        write_file(&file(&state, "identity"), &[1u8; 32]).unwrap();
        let _read_only = state_file::ReadOnly::make(&state).unwrap();
        migrate(&Fake::holding(None, Refuses::Nothing), &state, "identity")
            .map(drop)
            .unwrap_err();
    }
}
