#[path = "../crates/domyjob-core/src/chat/notification/stop.rs"]
mod stop;

#[kani::proof]
fn rejects_unacknowledged_urgent_stop() {
    kani::assert(
        stop::decide(true, stop::Acknowledgment::Pending) == stop::StopDecision::Allow,
        "NEGATIVE_CHAT_CONTROL",
    );
}
