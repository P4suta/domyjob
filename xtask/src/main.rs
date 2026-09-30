use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn gates(root: &Path, mut report: impl FnMut(&str)) -> ExitCode {
    match xtask::gates(root) {
        Ok(0) => {
            report("gates: clean");
            ExitCode::SUCCESS
        }
        Ok(count) => {
            report(&format!("gates: {count} finding(s)"));
            ExitCode::FAILURE
        }
        Err(error) => {
            report(&format!("gates: {error}"));
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

fn dispatch(root: &Path, words: &[&str]) -> ExitCode {
    match words {
        ["gates"] => gates(root, |message| eprintln!("{message}")),
        ["workflows"] => workflows(root),
        ["commit-msg", message] => commit_msg(root, Path::new(message)),
        ["dependencies", "deny"] => dependencies(root, xtask::dependencies::Check::Deny),
        ["dependencies", "audit"] => dependencies(root, xtask::dependencies::Check::Audit),
        ["dependencies", "vet"] => dependencies(root, xtask::dependencies::Check::Vet),
        _ => {
            eprintln!(
                "usage: cargo xtask gates | workflows | commit-msg FILE | dependencies deny|audit|vet"
            );
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    dispatch(&root, &words)
}

#[cfg(test)]
mod tests {
    use super::dispatch;
    use std::process::ExitCode;

    mod raw {
        #![expect(
            clippy::disallowed_methods,
            reason = "CLI tests write only their isolated repository fixtures"
        )]

        pub(super) fn create_dir_all(path: &std::path::Path) -> std::io::Result<()> {
            std::fs::create_dir_all(path)
        }

        pub(super) fn write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            std::fs::write(path, bytes)
        }
    }

    #[test]
    fn commands_preserve_success_failure_and_usage_exit_codes() {
        let root = tempfile::tempdir().unwrap();
        for args in [
            vec![],
            vec!["unknown"],
            vec!["dependencies", "unknown"],
            vec!["gates", "extra"],
        ] {
            assert_eq!(dispatch(root.path(), &args), ExitCode::from(2));
        }
        assert_eq!(dispatch(root.path(), &["gates"]), ExitCode::from(2));
        for directory in ["crates", "xtask"] {
            raw::create_dir_all(&root.path().join(directory)).unwrap();
        }
        assert_eq!(dispatch(root.path(), &["gates"]), ExitCode::SUCCESS);
        let mut diagnostics = Vec::new();
        assert_eq!(
            super::gates(root.path(), |message| diagnostics.push(message.to_owned())),
            ExitCode::SUCCESS
        );
        assert_eq!(diagnostics, ["gates: clean"]);
        raw::write(&root.path().join("settings.toml"), b"# comment\n").unwrap();
        assert_eq!(dispatch(root.path(), &["gates"]), ExitCode::FAILURE);
        for check in ["deny", "audit", "vet"] {
            assert_eq!(
                dispatch(root.path(), &["dependencies", check]),
                ExitCode::FAILURE
            );
        }
        assert_eq!(dispatch(root.path(), &["workflows"]), ExitCode::FAILURE);
        let message = root.path().join("COMMIT_EDITMSG");
        assert_eq!(
            dispatch(root.path(), &["commit-msg", message.to_str().unwrap()]),
            ExitCode::FAILURE
        );
        raw::write(&message, b"docs: update the manual\n").unwrap();
        assert_eq!(
            dispatch(root.path(), &["commit-msg", message.to_str().unwrap()]),
            ExitCode::SUCCESS
        );
        raw::write(&message, b"fix: update product code\n").unwrap();
        assert_eq!(
            dispatch(root.path(), &["commit-msg", message.to_str().unwrap()]),
            ExitCode::FAILURE
        );
    }
}
