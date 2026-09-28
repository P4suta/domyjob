#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "chat user wait deadlines and SSH polling intervals are explicit time limits, never job-state decisions"
)]

use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
pub(crate) struct WaitWindow {
    start: Instant,
    duration: Duration,
}

impl WaitWindow {
    pub(crate) fn new(seconds: u64) -> Self {
        Self {
            start: Instant::now(),
            duration: Duration::from_secs(seconds),
        }
    }

    pub(crate) fn expired(self) -> bool {
        self.start.elapsed() >= self.duration
    }

    pub(crate) fn wait_tick(self) {
        std::thread::sleep(INTERVAL.min(self.duration.saturating_sub(self.start.elapsed())));
    }
}

pub(crate) fn wait_tick() {
    std::thread::sleep(INTERVAL);
}
