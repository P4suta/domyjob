use domyjob_core::capture_policy::{CaptureAdmission, CapturePolicy, admit};

#[test]
fn only_diagnostic_prefixes_admit_an_omitted_suffix() {
    for omitted in [0, 1, u64::MAX] {
        assert_eq!(
            admit(CapturePolicy::Prefix, omitted),
            CaptureAdmission::Accept
        );
        assert_eq!(
            admit(CapturePolicy::Complete, omitted),
            if omitted == 0 {
                CaptureAdmission::Accept
            } else {
                CaptureAdmission::RefuseTruncated
            }
        );
    }
}
