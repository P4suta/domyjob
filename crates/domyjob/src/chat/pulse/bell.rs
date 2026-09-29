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

fn file(directory: &Path) -> PathBuf {
    directory.join("generation")
}

pub(super) fn ring(directory: &Path, generation: u64) -> Result<(), StateError> {
    state_io::write_bytes(&file(directory), generation.to_string().as_bytes())
}

pub(super) fn forget(directory: &Path) -> Result<(), StateError> {
    state_io::remove_file(&file(directory))
}

pub(super) fn generation(directory: &Path) -> Result<u64, BellError> {
    match state_io::read_bytes(&file(directory))? {
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

pub(super) struct Bell {
    _watcher: notify::RecommendedWatcher,
    receiver: mpsc::Receiver<Wake>,
    directory: PathBuf,
}

impl std::fmt::Debug for Bell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Bell")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl Bell {
    pub(super) fn watch(directory: &Path) -> Result<Self, BellError> {
        state_io::private_dir(directory)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut watcher = watch_event::watcher(move |notice| {
            let wake = match notice {
                Notice::Changed => Some(Wake::Changed),
                Notice::Unrelated => None,
                Notice::Failed(error) => Some(Wake::Broken(error.to_string())),
            };
            if let Some(wake) = wake {
                let _delivered = sender.try_send(wake);
            }
        })?;
        watcher.watch(directory, notify::RecursiveMode::NonRecursive)?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            directory: directory.to_path_buf(),
        })
    }

    pub(super) fn beyond(&self, known: u64, deadline: Deadline) -> Result<Option<u64>, BellError> {
        loop {
            let current = generation(&self.directory)?;
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
