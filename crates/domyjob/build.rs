#[path = "src/platform/file_kind.rs"]
mod file_kind;
#[path = "src/source_archive.rs"]
mod source_archive;
#[path = "src/source_fingerprint.rs"]
mod source_fingerprint;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn source_archive(root: &Path, files: Vec<PathBuf>) -> io::Result<Vec<u8>> {
    let mut archive = source_archive::Archive::new();
    for path in files {
        archive
            .add(root, &path, |_metadata| 0o644)
            .map_err(io::Error::other)?;
    }
    archive.finish().map_err(io::Error::other)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the build script writes generated source only inside Cargo OUT_DIR"
)]
fn main() -> io::Result<()> {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| io::Error::other("CARGO_MANIFEST_DIR is missing"))?;
    let manifest = Path::new(&manifest_dir);
    let checkout = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("the checkout root is missing"))?;
    let mut files = Vec::new();
    let fingerprint = source_fingerprint::from_checkout(checkout, |kind, path| {
        println!("cargo:rerun-if-changed={}", path.display());
        if kind == source_fingerprint::SourceKind::File {
            files.push(path.to_path_buf());
        }
    })?;
    let output =
        std::env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("OUT_DIR is missing"))?;
    let generated = Path::new(&output).join("fingerprint.rs");
    fs::write(
        generated,
        format!(
            "const COMPILED_FINGERPRINT: u64 = u64::from_be_bytes({:?});\n",
            fingerprint.to_be_bytes()
        ),
    )?;
    let archive = source_archive(checkout, files)?;
    if source_fingerprint::from_checkout(checkout, |_kind, _path| {})? != fingerprint {
        return Err(io::Error::other(
            "the source changed while building its archive",
        ));
    }
    fs::write(Path::new(&output).join("source.tar"), archive)?;
    Ok(())
}
