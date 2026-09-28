//! Wall-clock time for display and for bounding waits on other machines.
//!
//! No job, turn, or ledger decision reads the clock; it only limits how long a caller waits.
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "display timestamps, user wait limits, heartbeats, and retry pacing are explicit time limits"
)]

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn saturate(millis: u128) -> u64 {
    match u64::try_from(millis) {
        Ok(millis) => millis,
        Err(_too_large) => u64::MAX,
    }
}

/// Milliseconds since the Unix epoch, shown beside messages and link states.
#[must_use]
pub(crate) fn now_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => saturate(elapsed.as_millis()),
        Err(_before_epoch) => 0,
    }
}

/// A point in time after which a caller stops waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Deadline(Instant);

impl Deadline {
    #[must_use]
    pub(crate) fn after_millis(millis: u64) -> Self {
        let now = Instant::now();
        Self(
            now.checked_add(Duration::from_millis(millis))
                .unwrap_or(now),
        )
    }

    #[must_use]
    pub(crate) fn after_seconds(seconds: u64) -> Self {
        Self::after_millis(seconds.saturating_mul(1000))
    }

    #[must_use]
    pub(crate) fn expired(self) -> bool {
        Instant::now() >= self.0
    }

    /// The earlier of two deadlines.
    #[must_use]
    pub(crate) fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }
}

/// What waiting on a channel until a deadline produced.
#[derive(Debug)]
pub(crate) enum Waited<T> {
    Received(T),
    Expired,
    Closed,
}

/// Block on `receiver` until it yields a value, closes, or `deadline` passes.
pub(crate) fn receive<T>(receiver: &Receiver<T>, deadline: Deadline) -> Waited<T> {
    let left = deadline.0.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(left) {
        Ok(value) => Waited::Received(value),
        Err(RecvTimeoutError::Timeout) => Waited::Expired,
        Err(RecvTimeoutError::Disconnected) => Waited::Closed,
    }
}

/// Pause between retries of an unreachable peer.
pub(crate) fn pause_millis(millis: u64) {
    std::thread::sleep(Duration::from_millis(millis));
}
