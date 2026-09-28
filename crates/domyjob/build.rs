use std::path::PathBuf;

#[path = "src/build_config.rs"]
pub(crate) mod build_config;
#[path = "src/build_stamp.rs"]
pub(crate) mod build_stamp;

const EXPECTED_BUILD_STAMP: &str = "DOMYJOB_EXPECTED_BUILD_STAMP";

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-env-changed={EXPECTED_BUILD_STAMP}");
    let target = std::env::var("TARGET")
        .map_err(|error| std::io::Error::other(format!("TARGET is unavailable: {error}")))?;
    println!("cargo:rustc-env=DOMYJOB_TARGET={target}");
    let Some(crate_dir) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return Err(std::io::Error::other("CARGO_MANIFEST_DIR is unavailable"));
    };
    let Some(root) = crate_dir.parent().and_then(std::path::Path::parent) else {
        return Err(std::io::Error::other(format!(
            "{} has no workspace root two levels above it",
            crate_dir.display()
        )));
    };
    let (stamp, paths) = build_stamp::digest(root)?;
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    if let Ok(expected) = std::env::var(EXPECTED_BUILD_STAMP)
        && expected != stamp
    {
        return Err(std::io::Error::other(format!(
            "the source build stamp is {stamp}, not the expected {expected}"
        )));
    }
    println!("cargo:rustc-env=DOMYJOB_BUILD_STAMP={stamp}");
    Ok(())
}
