#[path = "src/source_fingerprint.rs"]
mod source_fingerprint;

#[expect(
    clippy::disallowed_methods,
    reason = "the build script writes generated source only inside Cargo OUT_DIR"
)]
fn main() -> std::io::Result<()> {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| std::io::Error::other("CARGO_MANIFEST_DIR is missing"))?;
    let manifest = std::path::Path::new(&manifest_dir);
    let checkout = manifest
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| std::io::Error::other("the checkout root is missing"))?;
    let fingerprint = source_fingerprint::from_checkout(checkout, |_kind, path| {
        println!("cargo:rerun-if-changed={}", path.display());
    })?;
    let output =
        std::env::var_os("OUT_DIR").ok_or_else(|| std::io::Error::other("OUT_DIR is missing"))?;
    let generated = std::path::Path::new(&output).join("fingerprint.rs");
    std::fs::write(
        generated,
        format!(
            "const COMPILED_FINGERPRINT: u64 = u64::from_be_bytes({:?});\n",
            fingerprint.to_be_bytes()
        ),
    )?;
    Ok(())
}
