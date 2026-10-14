use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
pub(super) struct MetadataDirectories {
    git: PathBuf,
    common: PathBuf,
}

impl MetadataDirectories {
    pub(super) const fn new(git: PathBuf, common: PathBuf) -> Self {
        Self { git, common }
    }

    pub(super) fn contains(&self, path: &Path) -> bool {
        contains(&self.git, &self.common, path)
    }
}

pub(super) fn contains(git: &Path, common: &Path, path: &Path) -> bool {
    contains_components(git.components(), common.components(), path.components())
}

pub(super) fn contains_components<'a>(
    git: impl Iterator<Item = Component<'a>>,
    common: impl Iterator<Item = Component<'a>>,
    path: impl Iterator<Item = Component<'a>> + Clone,
) -> bool {
    component_prefix(git, path.clone()) || component_prefix(common, path)
}

fn component_prefix<'a>(
    directory: impl Iterator<Item = Component<'a>>,
    mut path: impl Iterator<Item = Component<'a>>,
) -> bool {
    for component in directory {
        if path.next() != Some(component) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::MetadataDirectories;

    #[test]
    fn metadata_trees_use_complete_path_components() {
        let roots = MetadataDirectories::new(
            PathBuf::from("source/git data"),
            PathBuf::from("source/shared data"),
        );
        for path in [
            "source/git data",
            "source/git data/objects/private",
            "source/shared data/refs/private",
        ] {
            assert!(roots.contains(Path::new(path)));
        }
        for path in [
            "source",
            "source/git data source/public",
            "source/shared database/public",
            "source/working.txt",
        ] {
            assert!(!roots.contains(Path::new(path)));
        }
    }

    #[test]
    fn rooted_and_relative_component_names_cannot_alias() {
        let root = Path::new(std::path::MAIN_SEPARATOR_STR).join("source");
        let git = root.join("git data");
        let common = root.join("common data");
        let roots = MetadataDirectories::new(git.clone(), common);
        assert!(roots.contains(&git.join("private")));
        assert!(!roots.contains(Path::new("source/git data/private")));
        assert!(!roots.contains(&root));
    }
}
