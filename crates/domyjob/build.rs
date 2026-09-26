use std::path::{Path, PathBuf};

fn inputs(dir: &Path, root: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !matches!(
                name.to_str(),
                Some(".git" | ".jj" | "target" | "node_modules")
            ) {
                inputs(&path, root, found)?;
            }
        } else if kind.is_file()
            && matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("rs" | "toml" | "lock")
            )
        {
            let relative = path.strip_prefix(root).map_err(|error| {
                std::io::Error::other(format!(
                    "{} is not below {}: {error}",
                    path.display(),
                    root.display()
                ))
            })?;
            found.push(relative.to_path_buf());
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let target = std::env::var("TARGET")
        .map_err(|error| std::io::Error::other(format!("TARGET is unavailable: {error}")))?;
    println!("cargo:rustc-env=DOMYJOB_TARGET={target}");
    let Some(crate_dir) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return Err(std::io::Error::other("CARGO_MANIFEST_DIR is unavailable"));
    };
    let Some(root) = crate_dir.parent().and_then(Path::parent) else {
        return Err(std::io::Error::other(format!(
            "{} has no workspace root two levels above it",
            crate_dir.display()
        )));
    };
    let mut found = Vec::new();
    inputs(root, root, &mut found)?;
    found.sort();
    let mut digest = blake3::Hasher::new();
    for relative in found {
        let path = root.join(&relative);
        println!("cargo:rerun-if-changed={}", path.display());
        digest.update(relative.as_os_str().to_string_lossy().as_bytes());
        digest.update(&[0]);
        digest.update(&std::fs::read(path)?);
        digest.update(&[0]);
    }
    let hash = digest.finalize().to_hex();
    let Some(stamp) = hash.as_str().get(..16) else {
        return Err(std::io::Error::other(
            "the build digest was shorter than 16 ASCII characters",
        ));
    };
    println!("cargo:rustc-env=DOMYJOB_BUILD_STAMP={stamp}");
    Ok(())
}
