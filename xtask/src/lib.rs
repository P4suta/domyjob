use std::path::{Path, PathBuf};

pub mod comments;
pub mod release;
pub mod syntax;

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} does not parse: {source}")]
    Parse { path: PathBuf, source: syn::Error },
    #[error("usage: cargo xtask gates")]
    Usage,
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), GateError> {
    let entries = std::fs::read_dir(dir).map_err(|source| GateError::Read {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| GateError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let kind = entry.file_type().map_err(|source| GateError::Read {
            path: path.clone(),
            source,
        })?;
        if kind.is_dir() && name != "target" && name != ".git" {
            rust_files(&path, out)?;
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

const PLATFORM: &str = "crates/domyjob/src/platform.rs";

const NOT_YET_ONE_PATH: &[(&str, usize)] = &[
    ("crates/domyjob/src/keystore.rs", 5),
    ("crates/domyjob/src/proc.rs", 5),
    ("crates/domyjob/src/service.rs", 6),
    ("crates/domyjob/src/spawn.rs", 1),
    ("xtask/src/release.rs", 1),
];

fn os_findings(shown: &str, branches: usize) -> Option<String> {
    if shown == PLATFORM {
        return None;
    }
    let allowed = NOT_YET_ONE_PATH
        .iter()
        .find(|(file, _)| *file == shown)
        .map_or(0, |(_, allowed)| *allowed);
    match branches.cmp(&allowed) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(format!(
            "{shown}: {branches} operating-system branches where {allowed} are allowed; put the difference in {PLATFORM} and use one path everywhere else"
        )),
        std::cmp::Ordering::Less => Some(format!(
            "{shown}: now {branches} operating-system branches; lower its allowance in NOT_YET_ONE_PATH from {allowed} to {branches}"
        )),
    }
}

pub fn gates(root: &Path) -> Result<usize, GateError> {
    let mut files = Vec::new();
    for dir in ["crates", "xtask"] {
        rust_files(&root.join(dir), &mut files)?;
    }
    files.sort();
    let mut count = 0usize;
    for path in files {
        let source = std::fs::read_to_string(&path).map_err(|source| GateError::Read {
            path: path.clone(),
            source,
        })?;
        let shown = match path.strip_prefix(root) {
            Ok(inner) => inner.display().to_string(),
            Err(_outside) => path.display().to_string(),
        }
        .replace('\\', "/");
        let branches = syntax::os_branches(&source).map_err(|source| GateError::Parse {
            path: path.clone(),
            source,
        })?;
        if let Some(finding) = os_findings(&shown, branches) {
            eprintln!("{finding}");
            count = count.saturating_add(1);
        }
        for comment in comments::find(&source) {
            eprintln!(
                "{shown}:{}: comments are not written; put the reason in the commit message",
                comment.line
            );
            count = count.saturating_add(1);
        }
        let findings = syntax::check_file(&source, &shown).map_err(|source| GateError::Parse {
            path: path.clone(),
            source,
        })?;
        for finding in findings {
            eprintln!("{shown}:{}: {}", finding.line, finding.rule);
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}
