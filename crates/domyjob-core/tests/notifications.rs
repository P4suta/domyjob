use domyjob_core::chat::notification::{
    ActivityState, Delivery, QuestionState, ResponseDelivery, StopDecision, activity_state,
    escalation_due, reading_after_delivery, stop_decision,
};

#[test]
fn receipts_never_regress_and_urgent_stop_requires_acknowledgment() {
    for delivery in [ResponseDelivery::Original, ResponseDelivery::Replaced] {
        for cancelled in [false, true] {
            assert_eq!(
                reading_after_delivery(delivery, cancelled),
                delivery == ResponseDelivery::Original && !cancelled
            );
        }
    }
    let states = [
        Delivery::Pending,
        Delivery::Delivered,
        Delivery::Read,
        Delivery::Acknowledged,
    ];
    for current in states {
        for received in states {
            assert_eq!(current.observe(received), current.max(received));
        }
        assert_eq!(stop_decision(false, current), StopDecision::Allow);
        assert_eq!(
            stop_decision(true, current),
            if current == Delivery::Acknowledged {
                StopDecision::Allow
            } else {
                StopDecision::Block
            }
        );
    }
}

#[test]
fn deadlines_use_elapsed_time_without_overflow_or_clock_rollback() {
    assert!(!escalation_due(100, 20, 119, QuestionState::Waiting));
    assert!(escalation_due(100, 20, 120, QuestionState::Waiting));
    assert!(!escalation_due(100, 20, 99, QuestionState::Waiting));
    assert!(!escalation_due(100, 20, 120, QuestionState::Answered));
    assert!(!escalation_due(100, 20, 120, QuestionState::Escalated));
    assert!(!escalation_due(
        u64::MAX - 10,
        20,
        u64::MAX,
        QuestionState::Waiting
    ));
}

#[test]
fn presence_distinguishes_current_activity_from_stalled_activity() {
    assert_eq!(
        activity_state(ActivityState::Working, 100, 119, 20),
        ActivityState::Working
    );
    assert_eq!(
        activity_state(ActivityState::Waiting, 100, 119, 20),
        ActivityState::Waiting
    );
    assert_eq!(
        activity_state(ActivityState::Working, 100, 120, 20),
        ActivityState::Stalled
    );
    assert_eq!(
        activity_state(ActivityState::Waiting, 100, 99, 20),
        ActivityState::Waiting
    );
}
