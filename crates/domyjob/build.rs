#[path = "src/source_fingerprint.rs"]
mod source_fingerprint;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use domyjob_core::domain::RelativePath;
use domyjob_core::wire::MAX_SNAPSHOT_BYTES;

fn portable_name(root: &Path, path: &Path) -> io::Result<RelativePath> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_outside| io::Error::other("source is outside the checkout"))?;
    let mut components = Vec::new();
    for component in relative.components() {
        components.push(
            component
                .as_os_str()
                .to_str()
                .ok_or_else(|| io::Error::other("a source path is not UTF-8"))?,
        );
    }
    RelativePath::try_from(components.join("/"))
        .map_err(|_invalid| io::Error::other("a source path is not portable"))
}

fn source_archive(root: &Path, files: Vec<PathBuf>) -> io::Result<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut names = BTreeSet::new();
    for path in files {
        let name = portable_name(root, &path)?;
        if !names.insert(name.as_str().to_lowercase()) {
            return Err(io::Error::other("source paths collide by case"));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::other("a source path is not a regular file"));
        }
        let mut file = fs::File::open(&path)?;
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(metadata.len());
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        archive.append_data(&mut header, name.as_str(), &mut file)?;
        if file.read(&mut [0_u8; 1])? != 0 {
            return Err(io::Error::other("a source file changed during archiving"));
        }
    }
    let bytes = archive.into_inner()?;
    if u64::try_from(bytes.len()).map_err(io::Error::other)? > MAX_SNAPSHOT_BYTES {
        return Err(io::Error::other("the deployment source exceeds 64 MiB"));
    }
    Ok(bytes)
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
