//! Waiting for this machine's chat store to change.
//!
//! The doorbell is private to this module, so every wait goes through [`Pulse`].
//! Each wake-up first drives this machine's own progress:
//! it starts a worker for every agent with waiting asks,
//! and a new worker records a claim whose worker died as interrupted.
//! No wait can therefore outlive a worker that died during a turn.

mod bell;

pub(crate) use bell::BellError;

use super::runner::{self, RunnerError};
use super::store::Store;
use crate::layout;
use crate::platform::clock::Deadline;
use crate::state_io::StateError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum PulseError {
    #[error(transparent)]
    Bell(#[from] BellError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
}

/// Announce that the store at `paths` reached `generation`.
pub(crate) fn ring(paths: &layout::Chat, generation: u64) -> Result<(), StateError> {
    bell::ring(&paths.bell(), generation)
}

/// Forget the announced generations of a store that is being reset.
pub(crate) fn forget(paths: &layout::Chat) -> Result<(), StateError> {
    bell::forget(&paths.bell())
}

/// A wait on one store that drives this machine's progress at every wake-up.
#[derive(Debug)]
pub(crate) struct Pulse<'store> {
    store: &'store Store,
    bell: bell::Bell,
    generation: u64,
    tick_millis: u64,
}

impl<'store> Pulse<'store> {
    /// Start watching before the caller reads the state it waits for; wake at least every `tick_millis`.
    pub(crate) fn new(store: &'store Store, tick_millis: u64) -> Result<Self, PulseError> {
        let directory = store.paths().bell();
        let bell = bell::Bell::watch(&directory)?;
        let generation = bell::generation(&directory)?;
        Ok(Self {
            store,
            bell,
            generation,
            tick_millis,
        })
    }

    /// Drive this machine's progress, then wait for a change, the next tick, or `deadline`.
    ///
    /// Returns whether the store changed.
    pub(crate) fn next(&mut self, deadline: Deadline) -> Result<bool, PulseError> {
        runner::dispatch(self.store)?;
        let wake = deadline.min(Deadline::after_millis(self.tick_millis));
        match self.bell.beyond(self.generation, wake)? {
            Some(generation) => {
                self.generation = generation;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
