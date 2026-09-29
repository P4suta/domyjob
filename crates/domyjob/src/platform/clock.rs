use std::sync::mpsc::{Receiver, RecvTimeoutError};

use domyjob_core::chat::event::Stamp;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "the clock module owns every reading of time and every timed wait"
    )]

    use std::sync::OnceLock;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    static START: OnceLock<Instant> = OnceLock::new();

    pub(super) fn monotonic_millis() -> u128 {
        START.get_or_init(Instant::now).elapsed().as_millis()
    }

    pub(super) fn wall_millis() -> Option<u128> {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => Some(elapsed.as_millis()),
            Err(_before_epoch) => None,
        }
    }

    pub(super) fn receive<T>(receiver: &Receiver<T>, millis: u64) -> Result<T, RecvTimeoutError> {
        receiver.recv_timeout(Duration::from_millis(millis))
    }

    pub(super) fn sleep(millis: u64) {
        std::thread::sleep(Duration::from_millis(millis));
    }
}

fn saturate(millis: u128) -> u64 {
    match u64::try_from(millis) {
        Ok(millis) => millis,
        Err(_too_large) => u64::MAX,
    }
}

fn monotonic() -> u64 {
    saturate(raw::monotonic_millis())
}

#[must_use]
pub(crate) fn stamp() -> Stamp {
    Stamp::from_unix_millis(raw::wall_millis().map_or(0, saturate))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Deadline(u64);

impl Deadline {
    #[must_use]
    pub(crate) fn after_millis(millis: u64) -> Self {
        Self(monotonic().saturating_add(millis))
    }

    #[must_use]
    pub(crate) fn after_seconds(seconds: u64) -> Self {
        Self::after_millis(seconds.saturating_mul(1000))
    }

    #[must_use]
    pub(crate) fn expired(self) -> bool {
        monotonic() >= self.0
    }

    #[must_use]
    pub(crate) fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    fn left_millis(self) -> u64 {
        self.0.saturating_sub(monotonic())
    }
}

#[derive(Debug)]
pub(crate) enum Waited<T> {
    Received(T),
    Expired,
    Closed,
}

pub(crate) fn receive<T>(receiver: &Receiver<T>, deadline: Deadline) -> Waited<T> {
    match raw::receive(receiver, deadline.left_millis()) {
        Ok(value) => Waited::Received(value),
        Err(RecvTimeoutError::Timeout) => Waited::Expired,
        Err(RecvTimeoutError::Disconnected) => Waited::Closed,
    }
}

pub(crate) fn pause_millis(millis: u64) {
    raw::sleep(millis);
}

#[cfg(test)]
mod tests {
    use super::Deadline;

    #[test]
    fn a_deadline_too_far_away_never_expires() {
        assert!(!Deadline::after_millis(u64::MAX).expired());
        assert!(!Deadline::after_seconds(u64::MAX).expired());
        assert!(Deadline::after_millis(0).expired());
    }
}
