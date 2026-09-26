use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("{action} {path}: {source}")]
pub struct IoFailure {
    pub action: &'static str,
    pub path: PathBuf,
    pub source: std::io::Error,
}

impl IoFailure {
    #[must_use]
    pub fn kind(&self) -> std::io::ErrorKind {
        self.source.kind()
    }
}

pub fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> IoFailure + use<> {
    let path = path.to_path_buf();
    move |source| IoFailure {
        action,
        path,
        source,
    }
}
