#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Positive,
    NamedNegative,
}

const SUCCESS: i32 = 0;
const COUNTEREXAMPLE: i32 = 1;

pub(super) const fn admitted(phase: Phase, code: Option<i32>) -> bool {
    match (phase, code) {
        (Phase::Positive, Some(SUCCESS)) | (Phase::NamedNegative, Some(COUNTEREXAMPLE)) => true,
        (Phase::Positive | Phase::NamedNegative, None | Some(_)) => false,
    }
}
