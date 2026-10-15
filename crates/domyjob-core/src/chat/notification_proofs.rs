use super::notification::{
    ActivityState, Delivery, QuestionState, ResponseDelivery, StopDecision, activity_state,
    escalation_due, reading_after_delivery, stop_decision, summary_fits,
};

fn delivery(value: u8) -> Delivery {
    const STAGES: &[Delivery] = &[
        Delivery::Pending,
        Delivery::Delivered,
        Delivery::Read,
        Delivery::Acknowledged,
    ];
    STAGES[usize::from(value) % STAGES.len()]
}

#[kani::proof]
fn receipts_are_monotonic_and_convergent() {
    let original: bool = kani::any();
    let cancelled: bool = kani::any();
    let response = if original {
        ResponseDelivery::Original
    } else {
        ResponseDelivery::Replaced
    };
    assert_eq!(
        reading_after_delivery(response, cancelled),
        original && !cancelled
    );
    let current = delivery(kani::any());
    let received = delivery(kani::any());
    let third = delivery(kani::any());
    let next = current.observe(received);
    assert_eq!(next, current.max(received));
    assert_eq!(next, received.observe(current));
    assert_eq!(
        next.observe(third),
        current.observe(received.observe(third))
    );
    assert_eq!(current.observe(current), current);
    kani::cover!(current == Delivery::Pending && next == Delivery::Delivered);
    kani::cover!(current == Delivery::Delivered && next == Delivery::Read);
    kani::cover!(current == Delivery::Read && next == Delivery::Acknowledged);
    kani::cover!(current == Delivery::Acknowledged && next == Delivery::Acknowledged);
}

#[kani::proof]
fn urgent_stop_requires_acknowledgment() {
    let urgent: bool = kani::any();
    let stage = delivery(kani::any());
    let decision = stop_decision(urgent, stage);
    assert_eq!(
        decision == StopDecision::Block,
        urgent && stage != Delivery::Acknowledged
    );
    kani::cover!(urgent && stage == Delivery::Read && decision == StopDecision::Block);
    kani::cover!(urgent && stage == Delivery::Acknowledged && decision == StopDecision::Allow);
    kani::cover!(!urgent && stage == Delivery::Pending && decision == StopDecision::Allow);
}

#[kani::proof]
fn escalation_respects_deadlines_and_terminal_states() {
    let created: u64 = kani::any();
    let wait: u64 = kani::any();
    let now: u64 = kani::any();
    const STATES: &[QuestionState] = &[
        QuestionState::Waiting,
        QuestionState::Answered,
        QuestionState::Escalated,
    ];
    let state = STATES[usize::from(kani::any::<u8>()) % STATES.len()];
    let due = escalation_due(created, wait, now, state);
    assert_eq!(
        due,
        state == QuestionState::Waiting
            && now
                .checked_sub(created)
                .is_some_and(|elapsed| elapsed >= wait)
    );
    kani::cover!(due);
    kani::cover!(state == QuestionState::Waiting && now < created && !due);
    kani::cover!(state == QuestionState::Answered && !due);
    kani::cover!(state == QuestionState::Escalated && !due);
}

#[kani::proof]
fn presence_becomes_stalled_at_the_elapsed_bound() {
    let latest: u64 = kani::any();
    let now: u64 = kani::any();
    let stale_after: u64 = kani::any();
    const STATES: &[ActivityState] = &[
        ActivityState::Working,
        ActivityState::Waiting,
        ActivityState::Stalled,
    ];
    let state = STATES[usize::from(kani::any::<u8>()) % STATES.len()];
    let observed = activity_state(state, latest, now, stale_after);
    assert_eq!(
        observed,
        if now.saturating_sub(latest) >= stale_after {
            ActivityState::Stalled
        } else {
            state
        }
    );
    kani::cover!(state == ActivityState::Working && observed == ActivityState::Stalled);
    kani::cover!(state == ActivityState::Working && observed == ActivityState::Working);
    kani::cover!(state == ActivityState::Waiting && observed == ActivityState::Waiting);
}

#[kani::proof]
fn context_budget_rejects_overflow_and_excess() {
    let used: usize = kani::any();
    let next: usize = kani::any();
    let limit: usize = kani::any();
    let fits = summary_fits(used, next, limit);
    assert_eq!(fits, next <= limit && used <= limit.saturating_sub(next));
    kani::cover!(fits && used != 0 && next != 0);
    kani::cover!(!fits && used.checked_add(next).is_none());
    kani::cover!(!fits && used.checked_add(next).is_some());
}
