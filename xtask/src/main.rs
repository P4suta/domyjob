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

fn dependencies(root: &Path, check: xtask::dependencies::Check) -> ExitCode {
    match xtask::dependencies::run(root, check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dependencies: {error}");
            ExitCode::FAILURE
        }
    }
}

fn workflows(root: &Path) -> ExitCode {
    match xtask::workflows::check(root) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("workflows: {error}");
            ExitCode::FAILURE
        }
    }
}

fn commit_msg(root: &Path, message: &Path) -> ExitCode {
    match xtask::fixes::commit_msg(root, message) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("commit-msg: {error}");
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
        ["workflows"] => workflows(&root),
        ["commit-msg", message] => commit_msg(&root, Path::new(message)),
        ["dependencies", "deny"] => dependencies(&root, xtask::dependencies::Check::Deny),
        ["dependencies", "audit"] => dependencies(&root, xtask::dependencies::Check::Audit),
        ["dependencies", "vet"] => dependencies(&root, xtask::dependencies::Check::Vet),
        _ => {
            eprintln!(
                "usage: cargo xtask gates | workflows | commit-msg FILE | dependencies deny|audit|vet"
            );
            ExitCode::from(2)
        }
    }
}
