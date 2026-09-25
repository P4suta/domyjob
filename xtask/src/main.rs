use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn gates(root: &Path) -> ExitCode {
    match xtask::gates(root) {
        Ok(0) => {
            eprintln!("gates: clean");
            ExitCode::SUCCESS
        }
        Ok(count) => {
            eprintln!("gates: {count} finding(s)");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("gates: {error}");
            ExitCode::from(2)
        }
    }
}

fn keygen(path: &Path) -> ExitCode {
    match xtask::release::keygen(path) {
        Ok(public) => {
            println!(
                "wrote the release secret to {}; keep it offline",
                path.display()
            );
            println!("add this to RELEASE_KEYS in crates/domyjob/src/dist.rs:");
            println!(
                "    ReleaseKey {{\n        minisign: \"{}\",\n        ml_dsa: \"{}\",\n    }},",
                public.minisign, public.ml_dsa
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("release-keygen: {error}");
            ExitCode::FAILURE
        }
    }
}

fn sign(secret: &Path, manifest: &Path, trusted: &str) -> ExitCode {
    match xtask::release::sign(secret, manifest, trusted) {
        Ok(()) => {
            println!("signed {} with Ed25519 and ML-DSA-65", manifest.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("release-sign: {error}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["gates"] => gates(&root),
        ["release-keygen", path] => keygen(Path::new(path)),
        ["release-sign", secret, manifest, trusted] => {
            sign(Path::new(secret), Path::new(manifest), trusted)
        }
        _ => {
            eprintln!(
                "usage: cargo xtask gates | release-keygen SECRET-PATH | release-sign SECRET-PATH MANIFEST TRUSTED-COMMENT"
            );
            ExitCode::from(2)
        }
    }
}
