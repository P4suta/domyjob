#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use domyjob_core::wire::BuildId;

use crate::source_fingerprint;

include!(concat!(env!("OUT_DIR"), "/fingerprint.rs"));

pub(crate) const fn current() -> BuildId {
    BuildId::from_fingerprint(COMPILED_FINGERPRINT)
}

pub(crate) fn tag() -> String {
    format!("{COMPILED_FINGERPRINT:016x}")
}

pub(crate) fn checkout() -> Result<Option<BuildId>, std::io::Error> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| std::io::Error::other("the source checkout is unavailable"))?;
    match std::fs::symlink_metadata(root.join("crates/domyjob/src")) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_other) => {
            return Err(std::io::Error::other(
                "the source checkout is not a directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    Ok(Some(BuildId::from_fingerprint(
        source_fingerprint::from_checkout(root, |_kind, _path| {})?,
    )))
}
