use std::path::{Path, PathBuf};

struct ScanBudget {
    entries: usize,
    files: usize,
    path_bytes: usize,
    depth: usize,
}

impl ScanBudget {
    const fn source() -> Self {
        Self {
            entries: 200_000,
            files: 100_000,
            path_bytes: 16 << 20,
            depth: 64,
        }
    }

    fn take(remaining: &mut usize, count: usize, kind: &str) -> std::io::Result<()> {
        *remaining = remaining.checked_sub(count).ok_or_else(|| {
            std::io::Error::other(format!("build source exceeds its {kind} budget"))
        })?;
        Ok(())
    }
}

fn portable(path: &Path) -> std::io::Result<String> {
    let mut portable = String::new();
    for component in path.components() {
        if !portable.is_empty() {
            portable.push('/');
        }
        let name = component.as_os_str().to_str().ok_or_else(|| {
            std::io::Error::other(format!(
                "build source path is not Unicode: {}",
                path.display()
            ))
        })?;
        portable.push_str(name);
    }
    Ok(portable)
}

fn source_dir(path: &Path) -> bool {
    path == Path::new("crates")
        || path == Path::new("crates/domyjob")
        || path.starts_with("crates/domyjob/src")
        || path == Path::new(".cargo")
}

fn source_file(path: &Path) -> bool {
    path.starts_with("crates/domyjob/src")
        || [
            "Cargo.toml",
            "Cargo.lock",
            "mise.toml",
            "rust-toolchain.toml",
            ".cargo/config",
            ".cargo/config.toml",
            "crates/domyjob/Cargo.toml",
            "crates/domyjob/build.rs",
        ]
        .iter()
        .any(|name| path == Path::new(name))
}

struct SourceScan<'a> {
    root: &'a Path,
    budget: ScanBudget,
    found: Vec<(String, PathBuf)>,
}

impl SourceScan<'_> {
    fn inputs(&mut self, dir: &Path, depth: usize) -> std::io::Result<()> {
        if depth > self.budget.depth {
            return Err(std::io::Error::other(
                "build source exceeds its directory depth budget",
            ));
        }
        for entry in std::fs::read_dir(dir)? {
            ScanBudget::take(&mut self.budget.entries, 1, "directory entry")?;
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(self.root).map_err(|error| {
                std::io::Error::other(format!(
                    "{} is not below {}: {error}",
                    path.display(),
                    self.root.display()
                ))
            })?;
            let directory = source_dir(relative);
            let file = source_file(relative);
            if !directory && !file {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(std::io::Error::other(format!(
                    "build source contains a symbolic link: {}",
                    path.display()
                )));
            }
            if kind.is_dir() && directory {
                let child_depth = depth.checked_add(1).ok_or_else(|| {
                    std::io::Error::other("build source directory depth overflowed")
                })?;
                self.inputs(&path, child_depth)?;
            } else if kind.is_file() && file {
                let portable_name = portable(relative)?;
                ScanBudget::take(&mut self.budget.files, 1, "source file")?;
                ScanBudget::take(
                    &mut self.budget.path_bytes,
                    portable_name.len(),
                    "source path byte",
                )?;
                self.found.push((portable_name, relative.to_path_buf()));
            }
        }
        Ok(())
    }
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the same source file is compiled as a build-script module and a library module"
)]
pub(crate) fn digest(root: &Path) -> std::io::Result<(String, Vec<PathBuf>)> {
    let mut scan = SourceScan {
        root,
        budget: ScanBudget::source(),
        found: Vec::new(),
    };
    scan.inputs(root, 0)?;
    let mut found = scan.found;
    found.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = blake3::Hasher::new();
    let mut paths = Vec::with_capacity(found.len());
    let mut total = 0u64;
    for (portable, relative) in found {
        let path = root.join(&relative);
        digest.update(portable.as_bytes());
        digest.update(&[0]);
        let mut file = std::fs::File::open(&path)?;
        let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
        let mise = relative == Path::new("mise.toml");
        let mut mise_bytes = Vec::new();
        loop {
            let count = std::io::Read::read(&mut file, &mut buffer)?;
            if count == 0 {
                break;
            }
            let count_bytes = u64::try_from(count).map_err(std::io::Error::other)?;
            total = total
                .checked_add(count_bytes)
                .ok_or_else(|| std::io::Error::other("build source byte count overflowed"))?;
            if total > 64 << 20 {
                return Err(std::io::Error::other(
                    "build source exceeds its 64 MiB byte budget",
                ));
            }
            if let Some(chunk) = buffer.get(..count) {
                if mise {
                    if mise_bytes
                        .len()
                        .checked_add(count)
                        .is_none_or(|size| size > 1 << 20)
                    {
                        return Err(std::io::Error::other(
                            "mise.toml exceeds its 1 MiB parsing budget",
                        ));
                    }
                    mise_bytes.extend_from_slice(chunk);
                } else {
                    digest.update(chunk);
                }
            }
        }
        if mise {
            digest.update(crate::build_config::rust_tool(&mise_bytes)?.as_bytes());
        }
        digest.update(&[0]);
        paths.push(path);
    }
    let hash = digest.finalize().to_hex();
    let stamp = hash
        .as_str()
        .get(..16)
        .ok_or_else(|| {
            std::io::Error::other("the build digest was shorter than 16 ASCII characters")
        })?
        .to_owned();
    Ok((stamp, paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_listing_has_independent_entry_file_path_and_depth_budgets() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("crates/domyjob/src");
        crate::user_files::write(&source.join("one.rs"), b"").unwrap();
        crate::user_files::write(&source.join("two.rs"), b"").unwrap();
        crate::user_files::parents(&tmp.path().join("ignored").join("nested")).unwrap();
        for (budget, message) in [
            (
                ScanBudget {
                    entries: 1,
                    files: 10,
                    path_bytes: 100,
                    depth: 10,
                },
                "directory entry",
            ),
            (
                ScanBudget {
                    entries: 10,
                    files: 1,
                    path_bytes: 100,
                    depth: 10,
                },
                "source file",
            ),
            (
                ScanBudget {
                    entries: 10,
                    files: 10,
                    path_bytes: 1,
                    depth: 10,
                },
                "source path byte",
            ),
            (
                ScanBudget {
                    entries: 10,
                    files: 10,
                    path_bytes: 100,
                    depth: 0,
                },
                "directory depth",
            ),
        ] {
            let mut scan = SourceScan {
                root: tmp.path(),
                budget,
                found: Vec::new(),
            };
            let error = scan.inputs(tmp.path(), 0).unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn unrelated_repository_files_do_not_change_the_binary_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main = root.join("crates/domyjob/src/main.rs");
        crate::user_files::write(&main, b"fn main() {}\n").unwrap();
        let (original, paths) = digest(root).unwrap();
        assert_eq!(paths.as_slice(), std::slice::from_ref(&main));
        crate::user_files::write(&root.join("_typos.toml"), b"[default]\n").unwrap();
        crate::user_files::write(&root.join("xtask/src/main.rs"), b"fn main() {}\n").unwrap();
        assert_eq!(digest(root).unwrap().0, original);
        crate::user_files::write(&root.join("Cargo.toml"), b"[workspace]\n").unwrap();
        let configured = digest(root).unwrap().0;
        assert_ne!(configured, original);
        crate::user_files::write(&main, b"fn main() { println!(\"changed\"); }\n").unwrap();
        assert_ne!(digest(root).unwrap().0, configured);
    }

    #[test]
    fn mise_tasks_do_not_change_the_binary_stamp_but_rust_tool_does() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main = root.join("crates/domyjob/src/main.rs");
        let mise = root.join("mise.toml");
        crate::user_files::write(&main, b"fn main() {}\n").unwrap();
        crate::user_files::write(
            &mise,
            b"[tools]\nrust = \"1.98.0\"\n[tasks.one]\nrun = \"true\"\n",
        )
        .unwrap();
        let initial = digest(root).unwrap().0;
        crate::user_files::write(
            &mise,
            b"[tools]\nrust = \"1.98.0\"\n[tasks.two]\nrun = \"false\"\n",
        )
        .unwrap();
        assert_eq!(digest(root).unwrap().0, initial);
        crate::user_files::write(
            &mise,
            b"[tools]\nrust = \"1.99.0\"\n[tasks.two]\nrun = \"false\"\n",
        )
        .unwrap();
        assert_ne!(digest(root).unwrap().0, initial);
    }
}
