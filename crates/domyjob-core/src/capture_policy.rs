#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePolicy {
    Prefix,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureAdmission {
    Accept,
    RefuseTruncated,
}

#[must_use]
pub const fn admit(policy: CapturePolicy, omitted: u64) -> CaptureAdmission {
    match policy {
        CapturePolicy::Prefix => CaptureAdmission::Accept,
        CapturePolicy::Complete if omitted == 0 => CaptureAdmission::Accept,
        CapturePolicy::Complete => CaptureAdmission::RefuseTruncated,
    }
}

#[cfg(kani)]
mod proofs {
    use super::{CaptureAdmission, CapturePolicy, admit};

    #[kani::proof]
    fn complete_capture_rejects_every_omitted_suffix() {
        let omitted: u64 = kani::any();
        let policy = if kani::any::<bool>() {
            CapturePolicy::Prefix
        } else {
            CapturePolicy::Complete
        };
        let decision = admit(policy, omitted);
        assert_eq!(
            decision == CaptureAdmission::Accept,
            policy == CapturePolicy::Prefix || omitted == 0
        );
        kani::cover!(
            policy == CapturePolicy::Prefix && omitted == 0 && decision == CaptureAdmission::Accept
        );
        kani::cover!(
            policy == CapturePolicy::Prefix && omitted > 0 && decision == CaptureAdmission::Accept
        );
        kani::cover!(
            policy == CapturePolicy::Complete
                && omitted == 0
                && decision == CaptureAdmission::Accept
        );
        kani::cover!(
            policy == CapturePolicy::Complete
                && omitted > 0
                && decision == CaptureAdmission::RefuseTruncated
        );
    }

    #[kani::proof]
    fn rejects_truncated_complete_capture() {
        kani::assert(
            admit(CapturePolicy::Complete, 1) == CaptureAdmission::Accept,
            "NEGATIVE_TOOL_CAPTURE_CONTROL",
        );
    }
}
