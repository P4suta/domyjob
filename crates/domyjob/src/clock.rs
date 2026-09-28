use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "the one place the wall clock is read, and only as a record for people"
    )]
    pub fn observe() -> Self {
        let now = std::time::SystemTime::now();
        match now.duration_since(std::time::UNIX_EPOCH) {
            Ok(after) => Self(signed(after.as_millis())),
            Err(before) => Self(signed(before.duration().as_millis()).saturating_neg()),
        }
    }

    #[must_use]
    pub const fn until(self, later: Self) -> Elapsed {
        Elapsed(later.0.saturating_sub(self.0))
    }

    #[must_use]
    pub fn ago(self, now: Self) -> String {
        let seconds = self.until(now).0 / 1000;
        match seconds {
            ..10 => "just now".to_owned(),
            10..60 => format!("{seconds}s ago"),
            60..3600 => format!("{}m ago", seconds / 60),
            3600..86_400 => format!("{}h ago", seconds / 3600),
            _ => self.local_date(),
        }
    }

    #[cfg(test)]
    #[must_use]
    pub const fn at_millis(millis: i64) -> Self {
        Self(millis)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match jiff::Timestamp::from_millisecond(self.0) {
            Ok(at) => write!(f, "{}", at.strftime("%Y-%m-%dT%H:%M:%SZ")),
            Err(_out_of_range) => write!(f, "{}ms", self.0),
        }
    }
}

fn signed(millis: u128) -> i64 {
    match i64::try_from(millis) {
        Ok(fits) => fits,
        Err(_beyond_i64) => i64::MAX,
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
pub struct Elapsed(i64);

impl Elapsed {
    #[must_use]
    pub fn upper_median(samples: &mut [Self]) -> Option<Self> {
        samples.sort_unstable_by_key(|span| span.0);
        samples.get(samples.len() / 2).copied()
    }
}

impl Timestamp {
    #[must_use]
    pub fn local_date(self) -> String {
        match jiff::Timestamp::from_millisecond(self.0) {
            Ok(at) => at
                .to_zoned(jiff::tz::TimeZone::system())
                .strftime("%b %d %H:%M")
                .to_string(),
            Err(_out_of_range) => self.to_string(),
        }
    }
}

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let secs = self.0.unsigned_abs() / 1000;
        match secs {
            0..60 => write!(f, "{sign}{secs}s"),
            60..3600 => write!(f, "{sign}{}m{:02}s", secs / 60, secs % 60),
            3600..86_400 => write!(f, "{sign}{}h{:02}m", secs / 3600, (secs % 3600) / 60),
            _ => write!(f, "{sign}{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamps_print_as_utc_calendar_times() {
        assert_eq!(Timestamp::at_millis(0).to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(
            Timestamp::at_millis(1_790_245_551_278).to_string(),
            "2026-09-24T10:25:51Z"
        );
        assert_eq!(
            Timestamp::at_millis(951_782_400_000).to_string(),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            Timestamp::at_millis(-1000).to_string(),
            "1969-12-31T23:59:59Z"
        );
    }

    use super::*;

    #[test]
    fn elapsed_time_reads_naturally_and_never_decides_anything() {
        let start = Timestamp::at_millis(0);
        assert_eq!(start.until(Timestamp::at_millis(12_345)).to_string(), "12s");
        assert_eq!(
            start.until(Timestamp::at_millis(125_000)).to_string(),
            "2m05s"
        );
        assert_eq!(
            start.until(Timestamp::at_millis(7_260_000)).to_string(),
            "2h01m"
        );
        assert_eq!(
            Timestamp::at_millis(5000)
                .until(Timestamp::at_millis(0))
                .to_string(),
            "-5s"
        );
    }

    #[test]
    fn presentation_keeps_elapsed_values_opaque() {
        let start = Timestamp::at_millis(0);
        let mut samples = [
            start.until(Timestamp::at_millis(30_000)),
            start.until(Timestamp::at_millis(10_000)),
            start.until(Timestamp::at_millis(20_000)),
        ];
        let median = Elapsed::upper_median(&mut samples).unwrap();
        assert_eq!(median.to_string(), "20s");
        assert_eq!(serde_json::to_value(median).unwrap(), 20_000);
        assert_eq!(start.ago(Timestamp::at_millis(10_000)), "10s ago");
    }
}
