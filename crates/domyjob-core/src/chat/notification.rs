use serde::{Deserialize, Serialize};

use super::event::{Members, Stamp};
use super::id::{AgentId, Invalid, Line};
mod stop;
pub use stop::StopDecision;

pub const MAX_FALLBACK_SECONDS: u64 = 600;
pub const MILLIS_PER_SECOND: u64 = 1000;
pub const STALLED_MILLIS: u64 = 120 * MILLIS_PER_SECOND;
pub const MAX_CONTEXT_BYTES: usize = 4096;
pub const MAX_SUMMARY_BYTES: usize = 512;
pub const MAX_CONTEXT_MESSAGES: usize = 8;
pub const MAX_STATUS_BYTES: usize = 128;
pub const MAX_SUBJECT_BYTES: usize = 128;
pub const MAX_EXPECTED_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Pending,
    Delivered,
    Read,
    Acknowledged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseDelivery {
    Original,
    Replaced,
}

#[must_use]
pub const fn reading_after_delivery(delivery: ResponseDelivery, cancelled: bool) -> bool {
    matches!(delivery, ResponseDelivery::Original) && !cancelled
}

impl Delivery {
    const PENDING_RANK: u8 = 0;
    const DELIVERED_RANK: u8 = 1;
    const READ_RANK: u8 = 2;
    const ACKNOWLEDGED_RANK: u8 = 3;

    #[must_use]
    pub const fn observe(self, receipt: Self) -> Self {
        if self.rank() >= receipt.rank() {
            self
        } else {
            receipt
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Pending => Self::PENDING_RANK,
            Self::Delivered => Self::DELIVERED_RANK,
            Self::Read => Self::READ_RANK,
            Self::Acknowledged => Self::ACKNOWLEDGED_RANK,
        }
    }
}

#[must_use]
pub const fn stop_decision(urgent: bool, delivery: Delivery) -> StopDecision {
    let acknowledgment = if matches!(delivery, Delivery::Acknowledged) {
        stop::Acknowledgment::Confirmed
    } else {
        stop::Acknowledgment::Pending
    };
    stop::decide(urgent, acknowledgment)
}

#[must_use]
pub const fn escalation_due(
    created: u64,
    wait_millis: u64,
    now: u64,
    state: QuestionState,
) -> bool {
    matches!(state, QuestionState::Waiting)
        && matches!(now.checked_sub(created), Some(elapsed) if elapsed >= wait_millis)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionState {
    Waiting,
    Answered,
    Escalated,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Working,
    #[default]
    Waiting,
    Stalled,
}

#[must_use]
pub const fn activity_state(
    state: ActivityState,
    latest: u64,
    now: u64,
    stale_after: u64,
) -> ActivityState {
    if now.saturating_sub(latest) >= stale_after {
        ActivityState::Stalled
    } else {
        state
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    pub state: ActivityState,
    pub at: Stamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Line<MAX_STATUS_BYTES>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawQuestion", into = "RawQuestion")]
pub struct Question {
    expected: Line<MAX_EXPECTED_BYTES>,
    after_seconds: u64,
    fallback: AgentId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuestion {
    expected: Line<MAX_EXPECTED_BYTES>,
    after_seconds: u64,
    fallback: AgentId,
}

impl TryFrom<RawQuestion> for Question {
    type Error = Invalid;

    fn try_from(value: RawQuestion) -> Result<Self, Invalid> {
        Self::new(value.expected, value.after_seconds, value.fallback)
    }
}

impl From<Question> for RawQuestion {
    fn from(value: Question) -> Self {
        Self {
            expected: value.expected,
            after_seconds: value.after_seconds,
            fallback: value.fallback,
        }
    }
}

impl Question {
    pub fn new(
        expected: Line<MAX_EXPECTED_BYTES>,
        after_seconds: u64,
        fallback: AgentId,
    ) -> Result<Self, Invalid> {
        if !(1..=MAX_FALLBACK_SECONDS).contains(&after_seconds) {
            return Err(Invalid("question fallback delay"));
        }
        Ok(Self {
            expected,
            after_seconds,
            fallback,
        })
    }

    #[must_use]
    pub const fn expected(&self) -> &Line<MAX_EXPECTED_BYTES> {
        &self.expected
    }

    #[must_use]
    pub const fn fallback(&self) -> &AgentId {
        &self.fallback
    }

    #[must_use]
    pub const fn wait_millis(&self) -> u64 {
        self.after_seconds.saturating_mul(MILLIS_PER_SECOND)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub recipients: Members,
    #[serde(default)]
    pub urgent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Line<MAX_SUBJECT_BYTES>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
}

#[must_use]
pub const fn summary_fits(used: usize, next: usize, limit: usize) -> bool {
    matches!(used.checked_add(next), Some(total) if total <= limit)
}
