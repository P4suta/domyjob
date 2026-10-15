#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopDecision {
    Allow,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Acknowledgment {
    Pending,
    Confirmed,
}

#[must_use]
pub(super) const fn decide(urgent: bool, acknowledgment: Acknowledgment) -> StopDecision {
    if urgent && matches!(acknowledgment, Acknowledgment::Pending) {
        StopDecision::Block
    } else {
        StopDecision::Allow
    }
}
