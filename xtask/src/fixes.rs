//! A fix is committed only together with a test that would have caught it.
//!
//! A commit of type `fix` that changes product code must also change a test:
//! a test file or module, a fuzz seed, a format specimen, or a line that adds a test or an assertion.
//! A fix that exists only as code is lost in the next rewrite, as an archive timestamp fix once was.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum FixError {
    #[error("reading the commit message: {0}")]
    Message(std::io::Error),
    #[error("asking git for the staged change: {0}")]
    Git(std::io::Error),
    #[error("git failed: {0}")]
    GitFailed(String),
    #[error(
        "a `fix` that changes product code needs a test that fails without it; \
         stage the test with the fix, or name the commit for what it is"
    )]
    Untested,
}

/// Whether the commit message's subject has the type `fix`.
fn is_fix(message: &str) -> bool {
    let subject = message
        .lines()
        .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        .unwrap_or_default();
    let Some((kind, _)) = subject.split_once(':') else {
        return false;
    };
    let kind = kind.trim_end_matches('!');
    kind == "fix" || (kind.starts_with("fix(") && kind.ends_with(')'))
}

fn product(path: &str) -> bool {
    path.starts_with("crates/")
        && path.contains("/src/")
        && Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "rs")
        && !test_path(path)
}

fn test_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.ends_with("/tests.rs")
        || path.ends_with("/testing.rs")
        || path.ends_with("/specimen.rs")
        || path.ends_with("/exhaustive.rs")
        || path.starts_with("fuzz/")
        || path.starts_with("crates/domyjob/formats/")
}

/// Whether an added line of a staged diff adds a test or an assertion.
fn adds_a_check(line: &str) -> bool {
    line.starts_with('+')
        && !line.starts_with("+++")
        && (line.contains("#[test]") || line.contains("assert"))
}

/// Refuse an untested fix, judged from the message and the staged change.
pub fn check(message: &str, staged: &[&str], diff: &str) -> Result<(), FixError> {
    if !is_fix(message) || !staged.iter().any(|path| product(path)) {
        return Ok(());
    }
    if staged.iter().any(|path| test_path(path)) || diff.lines().any(adds_a_check) {
        return Ok(());
    }
    Err(FixError::Untested)
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, FixError> {
    let output = crate::raw::command("git")
        .current_dir(root)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(std::process::Child::wait_with_output)
        .map_err(FixError::Git)?;
    if !output.status.success() {
        return Err(FixError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The commit-msg hook: check the message at `path` against the staged change in `root`.
pub fn commit_msg(root: &Path, path: &Path) -> Result<(), FixError> {
    let message = crate::raw::read_to_string(path).map_err(FixError::Message)?;
    if !is_fix(&message) {
        return Ok(());
    }
    let names = git(root, &["diff", "--cached", "--name-only"])?;
    let staged: Vec<&str> = names.lines().collect();
    let diff = git(
        root,
        &["diff", "--cached", "--unified=0", "--", "crates", "fuzz"],
    )?;
    check(&message, &staged, &diff)
}

#[cfg(test)]
mod tests {
    use super::{FixError, check};

    #[test]
    fn a_fix_to_product_code_needs_a_test_beside_it() {
        let code = ["crates/domyjob/src/transport.rs"];
        assert!(matches!(
            check("fix: stop the race\n", &code, "+let a = 1;\n"),
            Err(FixError::Untested)
        ));
        assert!(matches!(
            check("fix(node)!: stop the race\n", &code, ""),
            Err(FixError::Untested)
        ));
        check("fix: stop the race\n", &code, "+    #[test]\n").unwrap();
        check(
            "fix: stop the race\n",
            &code,
            "+        assert_eq!(a, b);\n",
        )
        .unwrap();
        check(
            "fix: stop the race\n",
            &[
                "crates/domyjob/src/transport.rs",
                "crates/domyjob/tests/e2e/scenarios.rs",
            ],
            "",
        )
        .unwrap();
        check("feat: add a thing\n", &code, "").unwrap();
        check("fix: correct the docs\n", &["docs/chat.md"], "").unwrap();
        check("# comment\nfix: prefix only\n", &["README.md"], "").unwrap();
    }
}
