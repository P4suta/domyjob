use std::path::{Path, PathBuf};

use ed25519_dalek::Signer as _;
use ml_dsa::Keypair as _;

const MAGIC: &str = "domyjob release secret v1";
pub const ML_DSA_CONTEXT: &[u8] = b"domyjob release manifest v1";

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the system random source failed: {0}")]
    Random(getrandom::Error),
    #[error("{0} is not a domyjob release secret")]
    Malformed(PathBuf),
    #[error("ML-DSA signing failed")]
    Signing,
}

pub struct Secret {
    ed25519: [u8; 32],
    keynum: [u8; 8],
    ml_dsa: [u8; 32],
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret").finish_non_exhaustive()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.ed25519.fill(0);
        self.ml_dsa.fill(0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Public {
    pub minisign: String,
    pub ml_dsa: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub minisig: String,
    pub ml_dsa: String,
}

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = match data_encoding::HEXLOWER_PERMISSIVE.decode(text.as_bytes()) {
        Ok(bytes) => bytes,
        Err(_not_hex) => return None,
    };
    match <[u8; N]>::try_from(bytes.as_slice()) {
        Ok(array) => Some(array),
        Err(_wrong_length) => None,
    }
}

fn base64(bytes: &[u8]) -> String {
    data_encoding::BASE64.encode(bytes)
}

fn random<const N: usize>() -> Result<[u8; N], ReleaseError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(ReleaseError::Random)?;
    Ok(bytes)
}

impl Secret {
    pub fn generate() -> Result<Self, ReleaseError> {
        Ok(Self {
            ed25519: random()?,
            keynum: random()?,
            ml_dsa: random()?,
        })
    }

    fn ml_dsa_key(&self) -> ml_dsa::SigningKey<ml_dsa::MlDsa65> {
        ml_dsa::SigningKey::<ml_dsa::MlDsa65>::from_seed(&self.ml_dsa.into())
    }

    #[must_use]
    pub fn public(&self) -> Public {
        let ed = ed25519_dalek::SigningKey::from_bytes(&self.ed25519);
        let mut minisign = b"Ed".to_vec();
        minisign.extend_from_slice(&self.keynum);
        minisign.extend_from_slice(ed.verifying_key().as_bytes());
        Public {
            minisign: base64(&minisign),
            ml_dsa: hex(&self.ml_dsa_key().verifying_key().encode()),
        }
    }

    pub fn sign(&self, manifest: &[u8], trusted: &str) -> Result<Signed, ReleaseError> {
        use blake2::Digest as _;
        let ed = ed25519_dalek::SigningKey::from_bytes(&self.ed25519);
        let prehashed = blake2::Blake2b512::digest(manifest);
        let signature = ed.sign(&prehashed).to_bytes();
        let mut packed = b"ED".to_vec();
        packed.extend_from_slice(&self.keynum);
        packed.extend_from_slice(&signature);
        let mut global = signature.to_vec();
        global.extend_from_slice(trusted.as_bytes());
        let global = ed.sign(&global).to_bytes();
        let minisig = format!(
            "untrusted comment: signature from the domyjob release key\n{}\ntrusted comment: {trusted}\n{}\n",
            base64(&packed),
            base64(&global)
        );
        let quantum = self
            .ml_dsa_key()
            .expanded_key()
            .sign_deterministic(manifest, ML_DSA_CONTEXT)
            .map_err(|_too_long| ReleaseError::Signing)?;
        Ok(Signed {
            minisig,
            ml_dsa: format!("{}\n", hex(&quantum.encode())),
        })
    }

    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{MAGIC}\n{}\n{}\n{}\n",
            hex(&self.ed25519),
            hex(&self.keynum),
            hex(&self.ml_dsa)
        )
    }

    pub fn decode(text: &str, path: &Path) -> Result<Self, ReleaseError> {
        let malformed = || ReleaseError::Malformed(path.to_path_buf());
        let mut lines = text.lines();
        if lines.next() != Some(MAGIC) {
            return Err(malformed());
        }
        let mut field = || lines.next().ok_or_else(malformed);
        let ed25519 = unhex(field()?).ok_or_else(malformed)?;
        let keynum = unhex(field()?).ok_or_else(malformed)?;
        let ml_dsa = unhex(field()?).ok_or_else(malformed)?;
        Ok(Self {
            ed25519,
            keynum,
            ml_dsa,
        })
    }
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> ReleaseError + use<> {
    let path = path.to_path_buf();
    move |source| ReleaseError::Io {
        action,
        path,
        source,
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the release secret lives outside domyjob's state, created once and never overwritten"
)]
pub fn keygen(path: &Path) -> Result<Public, ReleaseError> {
    use std::io::Write as _;
    let secret = Secret::generate()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io("creating", path))?;
    file.write_all(secret.encode().as_bytes())
        .map_err(io("writing", path))?;
    Ok(secret.public())
}

#[expect(
    clippy::disallowed_methods,
    reason = "signatures are written next to the manifest they sign, for upload"
)]
pub fn sign(secret_path: &Path, manifest: &Path, trusted: &str) -> Result<(), ReleaseError> {
    let text = std::fs::read_to_string(secret_path).map_err(io("reading", secret_path))?;
    let secret = Secret::decode(&text, secret_path)?;
    let bytes = std::fs::read(manifest).map_err(io("reading", manifest))?;
    let signed = secret.sign(&bytes, trusted)?;
    for (suffix, content) in [(".minisig", &signed.minisig), (".mldsa", &signed.ml_dsa)] {
        let mut name = manifest.as_os_str().to_owned();
        name.push(suffix);
        let target = PathBuf::from(name);
        std::fs::write(&target, content).map_err(io("writing", &target))?;
    }
    Ok(())
}
