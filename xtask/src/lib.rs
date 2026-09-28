use std::path::{Path, PathBuf};

use syn::visit::Visit;

pub mod dependencies;
pub mod exceptions;
pub mod fixes;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "repository tasks run their pinned tools and write only their caches and fixtures"
    )]

    use std::io;
    use std::path::Path;
    use std::process::Command;

    pub(super) fn command(program: &str) -> Command {
        Command::new(program)
    }

    pub(super) fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    #[cfg(test)]
    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }
}
pub mod pure_core;
pub mod workflows;

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} does not parse: {source}")]
    Parse { path: PathBuf, source: syn::Error },
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), GateError> {
    for entry in std::fs::read_dir(dir).map_err(|source| GateError::Read {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| GateError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|source| GateError::Read {
            path: path.clone(),
            source,
        })?;
        if kind.is_dir() {
            rust_files(&path, out)?;
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct SourcePolicy {
    effect_module: bool,
    /// Whether this file may take the process's standard output.
    output_owner: bool,
    findings: Vec<String>,
}

impl<'ast> Visit<'ast> for SourcePolicy {
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        let line = attribute
            .path()
            .segments
            .first()
            .map_or(1, |part| part.ident.span().start().line);
        if attribute.path().is_ident("allow") {
            self.findings.push(format!(
                "{line}: use a reasoned expect attribute instead of allow"
            ));
        }
        let platform_branch = attribute.path().is_ident("cfg")
            && matches!(&attribute.meta, syn::Meta::List(list) if list.tokens.to_string() != "test")
            || attribute.path().is_ident("cfg_attr");
        if platform_branch && !self.effect_module {
            self.findings.push(format!(
                "{line}: platform branches belong in the platform or process adapter"
            ));
        }
        syn::visit::visit_attribute(self, attribute);
    }

    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        let names: Vec<String> = expression
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        if !self.output_owner && names.ends_with(&["Output".to_owned(), "of_process".to_owned()]) {
            let line = expression
                .path
                .segments
                .first()
                .map_or(1, |part| part.ident.span().start().line);
            self.findings.push(format!(
                "{line}: only `main` takes standard output; pass the `Output` it created"
            ));
        }
        syn::visit::visit_expr_path(self, expression);
    }
}

fn effect_module(path: &str) -> bool {
    matches!(
        path,
        "crates/domyjob/src/platform.rs" | "crates/domyjob/src/process.rs"
    ) || path.starts_with("crates/domyjob/src/platform/")
        || path.starts_with("crates/domyjob/src/process/")
}

pub fn gates(root: &Path) -> Result<usize, GateError> {
    let mut files = Vec::new();
    for directory in ["crates", "xtask"] {
        rust_files(&root.join(directory), &mut files)?;
    }
    files.sort();
    let mut findings = 0usize;
    for path in files {
        let source = std::fs::read_to_string(&path).map_err(|source| GateError::Read {
            path: path.clone(),
            source,
        })?;
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_outside) => &path,
        };
        let shown = relative.to_string_lossy().replace('\\', "/");
        let parsed = syn::parse_file(&source).map_err(|source| GateError::Parse {
            path: path.clone(),
            source,
        })?;
        if shown.starts_with("crates/domyjob-core/src/") {
            for finding in pure_core::check(&source, shown == "crates/domyjob-core/src/lib.rs")
                .map_err(|source| GateError::Parse {
                    path: path.clone(),
                    source,
                })?
            {
                eprintln!("{shown}:{finding}");
                findings = findings.saturating_add(1);
            }
        }
        let mut policy = SourcePolicy {
            effect_module: effect_module(&shown),
            output_owner: shown == "crates/domyjob/src/main.rs",
            findings: Vec::new(),
        };
        policy.visit_file(&parsed);
        policy.findings.extend(exceptions::check(&shown, &parsed));
        for finding in policy.findings {
            eprintln!("{shown}:{finding}");
            findings = findings.saturating_add(1);
        }
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::{SourcePolicy, effect_module};
    use syn::visit::Visit;

    #[test]
    fn platform_branches_and_allow_attributes_are_confined() {
        let source =
            syn::parse_file("#[cfg(windows)] fn platform() {} #[allow(dead_code)] fn hidden() {}")
                .unwrap();
        let mut policy = SourcePolicy::default();
        policy.visit_file(&source);
        assert_eq!(policy.findings.len(), 2);
        let printing = syn::parse_file("fn f() { let output = Output::of_process(); }").unwrap();
        let mut elsewhere = SourcePolicy::default();
        elsewhere.visit_file(&printing);
        assert_eq!(elsewhere.findings.len(), 1);
        assert!(effect_module("crates/domyjob/src/process/windows.rs"));
        assert!(!effect_module("crates/domyjob/src/store.rs"));
    }
}
