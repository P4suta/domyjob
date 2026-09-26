use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    #[error("{0:?} is not a job id: expected 16 Crockford base32 characters")]
    JobId(String),
    #[error("{0:?} is not a job reference: expected 1 to 16 Crockford base32 characters")]
    JobRef(String),
    #[error("{0:?} is not a full commit id")]
    Sha(String),
    #[error("{0:?} is not a project key")]
    ProjectKey(String),
    #[error("{0:?} is not a content digest")]
    BlobId(String),
    #[error("{0:?} is not a submission nonce")]
    Nonce(String),
    #[error("{0:?} is not an audit chain hash")]
    ChainHash(String),
    #[error("{0:?} is not a machine name")]
    MachineName(String),
    #[error(
        "{0:?} is not a relative path such as dir/file.txt (no leading /, no .., no drive, nothing inside version-control metadata)"
    )]
    RelPath(String),
    #[error("{0:?} is not an environment variable name")]
    EnvName(String),
    #[error("{0:?} is not a job name")]
    JobName(String),
    #[error("{0:?} is not a target triple")]
    TargetTriple(String),
    #[error("{0:?} is not a commit id")]
    CommitId(String),
    #[error("{0:?} is not a revision")]
    Revision(String),
    #[error("{0:?} is not a Windows security identifier")]
    WindowsSid(String),
    #[error(
        "{0:?} is not a host: it must not start with - or contain spaces or control characters"
    )]
    Host(String),
    #[error("{0:?} is not a pairing code: expected four words from the pairing word list")]
    PairingCode(String),
    #[error("{0:?} is not an exposure: use loopback, tailnet, lan, or ADDRESS:PORT")]
    Exposure(String),
    #[error("{0:?} is not a 32-byte hex key")]
    Key(String),
    #[error("{0} is not a concurrency: expected 1 to 64")]
    Concurrency(u32),
    #[error("the system random source failed: {0}")]
    Random(getrandom::Error),
}

const WINDOWS_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn portable_component(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    !part.ends_with('.') && !part.ends_with(' ') && !WINDOWS_DEVICES.contains(&stem.as_str())
}

pub const METADATA_DIRS: &[&str] = &[
    ".git", ".jj", ".hg", ".svn", ".pijul", "_darcs", ".bzr", "CVS",
];

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn crockford(text: &str) -> bool {
    text.bytes().all(|b| CROCKFORD.contains(&b))
}

macro_rules! text_newtype {
    ($name:ident, $check:expr, $variant:ident) => {
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
            schemars::JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl TryFrom<String> for $name {
            type Error = Invalid;

            fn try_from(value: String) -> Result<Self, Invalid> {
                let check: fn(&str) -> bool = $check;
                if check(&value) {
                    Ok(Self(value))
                } else {
                    Err(Invalid::$variant(value))
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = Invalid;

            fn from_str(value: &str) -> Result<Self, Invalid> {
                Self::try_from(value.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

text_newtype!(JobId, |s| s.len() == 16 && crockford(s), JobId);
text_newtype!(
    JobRef,
    |s| (1..=16).contains(&s.len()) && crockford(s),
    JobRef
);
text_newtype!(
    Sha,
    |s| matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
    Sha
);
text_newtype!(
    BlobId,
    |s| s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
    BlobId
);
text_newtype!(
    ChainHash,
    |s| s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
    ChainHash
);
text_newtype!(
    ProjectKey,
    |s| (1..=100).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !s.starts_with('-'),
    ProjectKey
);
text_newtype!(
    MachineName,
    |s| (1..=200).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@:[]".contains(&b)),
    MachineName
);
text_newtype!(
    TargetTriple,
    |s| (1..=80).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && !s.starts_with('-'),
    TargetTriple
);
text_newtype!(
    CommitId,
    |s| (4..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()),
    CommitId
);
text_newtype!(
    Revision,
    |s| (1..=200).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/@^~:-+".contains(&b)),
    Revision
);
text_newtype!(
    WindowsSid,
    |s| (5..=184).contains(&s.len())
        && s.starts_with("S-1-")
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S'),
    WindowsSid
);
text_newtype!(
    Host,
    |s| (1..=255).contains(&s.len())
        && !s.starts_with('-')
        && s.chars().all(|c| !c.is_whitespace() && !c.is_control()),
    Host
);
text_newtype!(
    Nonce,
    |s| s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
    Nonce
);
text_newtype!(
    RelPath,
    |s| !s.starts_with('/')
        && !s.starts_with('-')
        && !s.contains('\\')
        && !s.contains(':')
        && s.split('/').all(|part| part != ".." && part != ".")
        && !s.split('/').any(str::is_empty)
        && !s.contains(['<', '>', '"', '|', '?', '*'])
        && !s.chars().any(char::is_control)
        && s.split('/').all(portable_component)
        && !s.split('/').any(|part| METADATA_DIRS.contains(&part)),
    RelPath
);
text_newtype!(
    EnvName,
    |s| !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && !s.to_ascii_uppercase().starts_with("DOMYJOB")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
    EnvName
);
text_newtype!(
    JobName,
    |s| (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
    JobName
);

macro_rules! safe_word {
    ($($name:ident),*) => {
        $(impl crate::template::SafeWord for $name {
            fn safe_word(&self) -> &str {
                &self.0
            }
        })*
    };
}

safe_word!(
    WindowsSid,
    JobId,
    BlobId,
    Nonce,
    ProjectKey,
    MachineName,
    RelPath,
    TargetTriple,
    CommitId,
    Revision,
    Host
);

impl Nonce {
    pub fn generate() -> Result<Self, Invalid> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(Invalid::Random)?;
        Ok(Self(crate::trust::hex(&random)))
    }
}

const CROCKFORD_BASE32: data_encoding::Encoding = data_encoding_macro::new_encoding! {
    symbols: "0123456789ABCDEFGHJKMNPQRSTVWXYZ",
};

impl JobId {
    pub fn generate() -> Result<Self, Invalid> {
        let mut random = [0u8; 10];
        getrandom::fill(&mut random).map_err(Invalid::Random)?;
        Self::try_from(CROCKFORD_BASE32.encode(&random))
    }

    #[must_use]
    pub fn matches(&self, reference: &JobRef) -> bool {
        self.0.starts_with(reference.as_str())
    }
}

impl MachineName {
    #[must_use]
    pub fn to_host(&self) -> Host {
        Host(self.0.clone())
    }
}

impl From<JobId> for JobRef {
    fn from(value: JobId) -> Self {
        Self(value.0)
    }
}

impl JobRef {
    pub fn parse_loose(text: &str) -> Result<Self, Invalid> {
        Self::try_from(text.to_ascii_uppercase())
    }
}

impl BlobId {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    #[must_use]
    pub fn from_hash(hash: &blake3::Hash) -> Self {
        Self(hash.to_hex().to_string())
    }

    #[must_use]
    pub fn split(&self) -> (&str, &str) {
        self.0.split_at_checked(2).unwrap_or(("00", &self.0))
    }
}

impl RelPath {
    #[must_use]
    pub fn parts(&self) -> std::str::Split<'_, char> {
        self.0.split('/')
    }

    #[must_use]
    pub fn to_local(&self) -> std::path::PathBuf {
        self.parts().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "u32", into = "u32")]
pub struct Concurrency(NonZeroU32);

impl TryFrom<u32> for Concurrency {
    type Error = Invalid;

    fn try_from(value: u32) -> Result<Self, Invalid> {
        match NonZeroU32::new(value) {
            Some(n) if value <= Self::MOST => Ok(Self(n)),
            Some(_) | None => Err(Invalid::Concurrency(value)),
        }
    }
}

impl From<Concurrency> for u32 {
    fn from(value: Concurrency) -> Self {
        value.0.get()
    }
}

impl Concurrency {
    pub const MOST: u32 = 64;
    pub const DEFAULT: Self = Self(NonZeroU32::MIN.saturating_add(3));

    #[must_use]
    pub fn slots(self) -> usize {
        match usize::try_from(self.0.get()) {
            Ok(n) => n,
            Err(_beyond_usize) => usize::MAX,
        }
    }
}

#[must_use]
pub fn to_usize(value: u32) -> usize {
    match usize::try_from(value) {
        Ok(fits) => fits,
        Err(_beyond_usize) => usize::MAX,
    }
}

#[must_use]
pub fn len_u64(len: usize) -> u64 {
    match u64::try_from(len) {
        Ok(fits) => fits,
        Err(_beyond_u64) => u64::MAX,
    }
}

impl crate::ingress::Ingress for EnvName {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_are_random_and_valid() {
        let a = JobId::generate().unwrap();
        let b = JobId::generate().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 16);
        let prefix: String = a.as_str().chars().take(6).collect();
        assert!(a.matches(&JobRef::parse_loose(&prefix.to_lowercase()).unwrap()));
    }

    #[test]
    fn newtypes_refuse_bad_values() {
        "abc".parse::<Sha>().unwrap_err();
        "0123456789abcdef0123456789abcdef01234567"
            .parse::<Sha>()
            .unwrap();
        "0123456789ABCDEF0123456789ABCDEF01234567"
            .parse::<Sha>()
            .unwrap_err();
        "../x".parse::<RelPath>().unwrap_err();
        "a//b".parse::<RelPath>().unwrap_err();
        "/abs".parse::<RelPath>().unwrap_err();
        ".git/hooks/post-checkout".parse::<RelPath>().unwrap_err();
        "vendor/lib/.jj/repo".parse::<RelPath>().unwrap_err();
        ".github/workflows/ci.yml".parse::<RelPath>().unwrap();
        "crates/core".parse::<RelPath>().unwrap();
        "-oProxyCommand=x".parse::<MachineName>().unwrap_err();
        "me@build-box".parse::<MachineName>().unwrap();
        "1BAD".parse::<EnvName>().unwrap_err();
        "ILOU".parse::<JobRef>().unwrap_err();
    }

    #[test]
    fn paths_must_survive_every_operating_system() {
        for bad in [
            "CON",
            "dir/nul.txt",
            "aux",
            "trailing.",
            "space ",
            "-rf",
            "tab\tname",
            "a/../b",
            "c:d",
        ] {
            bad.parse::<RelPath>().unwrap_err();
        }
        for good in ["console.log", "src/lib.rs", "a/b c/d.txt", "nullable.rs"] {
            good.parse::<RelPath>().unwrap();
        }
        "x86_64-unknown-linux-gnu".parse::<TargetTriple>().unwrap();
        "x86_64 -o /tmp/x".parse::<TargetTriple>().unwrap_err();
        "--upload-pack=evil".parse::<Revision>().unwrap_err();
        "-oProxyCommand=x".parse::<Host>().unwrap_err();
        "DOMYJOB_JOB_ID".parse::<EnvName>().unwrap_err();
    }

    #[test]
    fn concurrency_is_bounded() {
        assert_eq!(Concurrency::DEFAULT.slots(), 4);
        Concurrency::try_from(0).unwrap_err();
        Concurrency::try_from(65).unwrap_err();
        assert_eq!(Concurrency::try_from(64).unwrap().slots(), 64);
    }

    proptest::proptest! {
        #[test]
        fn accepted_paths_never_leave_the_root(text in "[a-zA-Z0-9._/ \\:-]{1,40}") {
            if let Ok(path) = text.parse::<RelPath>() {
                for part in path.parts() {
                    proptest::prop_assert!(part != ".." && part != "." && !part.is_empty());
                }
                proptest::prop_assert!(!path.as_str().starts_with('/'));
                proptest::prop_assert!(!path.as_str().contains('\\') && !path.as_str().contains(':'));
            }
        }

        #[test]
        fn generated_job_ids_are_always_valid_and_prefix_resolvable(cut in 1usize..=16) {
            let id = JobId::generate().unwrap();
            let prefix: String = id.as_str().chars().take(cut).collect();
            proptest::prop_assert!(id.matches(&JobRef::parse_loose(&prefix.to_lowercase()).unwrap()));
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn concurrency_is_exactly_one_to_sixty_four() {
        let value: u32 = kani::any();
        match Concurrency::try_from(value) {
            Ok(accepted) => {
                assert!((1..=64).contains(&value));
                assert!(u32::try_from(accepted.slots()) == Ok(value));
            }
            Err(_) => {
                assert!(value == 0 || value > 64);
            }
        }
    }
}
