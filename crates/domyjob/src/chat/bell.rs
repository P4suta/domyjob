//! The doorbell: a generation number replaced after every commit that stores events.
//!
//! It lives alone in its directory, so watching it never observes database writes.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use notify::Watcher as _;

use crate::platform::clock::{self, Deadline, Waited};
use crate::state_io::{self, StateError};
use crate::watch_event::{self, Notice};

#[derive(Debug, thiserror::Error)]
pub(crate) enum BellError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error("watching the chat doorbell failed: {0}")]
    Watch(#[from] notify::Error),
    #[error("the chat doorbell is corrupt")]
    Corrupt,
    #[error("the chat doorbell watcher stopped: {0}")]
    Broken(String),
}

/// The doorbell directory inside a chat store root.
#[must_use]
pub(crate) fn directory(root: &Path) -> PathBuf {
    root.join("bell")
}

fn file(root: &Path) -> PathBuf {
    directory(root).join("generation")
}

/// Announce that the store reached `generation`.
pub(crate) fn ring(root: &Path, generation: u64) -> Result<(), StateError> {
    state_io::write_bytes(&file(root), generation.to_string().as_bytes())
}

/// The last announced generation, or zero before the first commit.
pub(crate) fn generation(root: &Path) -> Result<u64, BellError> {
    match state_io::read_bytes(&file(root))? {
        None => Ok(0),
        Some(bytes) => match std::str::from_utf8(&bytes).map(|text| text.trim().parse::<u64>()) {
            Ok(Ok(generation)) => Ok(generation),
            Ok(Err(_)) | Err(_) => Err(BellError::Corrupt),
        },
    }
}

enum Wake {
    Changed,
    Broken(String),
}

/// A registered watch on one store's doorbell.
pub(crate) struct Bell {
    _watcher: notify::RecommendedWatcher,
    receiver: mpsc::Receiver<Wake>,
    root: PathBuf,
}

impl std::fmt::Debug for Bell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Bell")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Bell {
    /// Start watching before the caller reads the state it waits to change.
    pub(crate) fn watch(root: &Path) -> Result<Self, BellError> {
        let directory = directory(root);
        state_io::private_dir(&directory)?;
        let (sender, receiver) = mpsc::channel();
        let mut watcher = watch_event::watcher(move |notice| {
            let wake = match notice {
                Notice::Changed => Some(Wake::Changed),
                Notice::Unrelated => None,
                Notice::Failed(error) => Some(Wake::Broken(error.to_string())),
            };
            if let Some(wake) = wake {
                let _delivered = sender.send(wake);
            }
        })?;
        watcher.watch(&directory, notify::RecursiveMode::NonRecursive)?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            root: root.to_path_buf(),
        })
    }

    /// Wait until the generation exceeds `known`; `None` when `deadline` passes first.
    pub(crate) fn beyond(&self, known: u64, deadline: Deadline) -> Result<Option<u64>, BellError> {
        loop {
            let current = generation(&self.root)?;
            if current > known {
                return Ok(Some(current));
            }
            match clock::receive(&self.receiver, deadline) {
                Waited::Received(Wake::Changed) => {}
                Waited::Received(Wake::Broken(detail)) => return Err(BellError::Broken(detail)),
                Waited::Expired => return Ok(None),
                Waited::Closed => return Err(BellError::Broken("watcher closed".to_owned())),
            }
        }
    }
}
