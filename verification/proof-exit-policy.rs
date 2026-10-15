#[path = "../xtask/src/resource_proofs/exit_policy.rs"]
mod exit_policy;

use exit_policy::{Phase, admitted};

const NORMAL_SUCCESS: i32 = 0;
const NORMAL_COUNTEREXAMPLE: i32 = 1;
const UNRELATED_FAILURE: i32 = 2;

#[kani::proof]
fn only_the_required_normal_code_admits_each_proof_phase() {
    let code: Option<i32> = kani::any();
    let phase = if kani::any() {
        Phase::Positive
    } else {
        Phase::NamedNegative
    };
    let expected = match phase {
        Phase::Positive => code == Some(NORMAL_SUCCESS),
        Phase::NamedNegative => code == Some(NORMAL_COUNTEREXAMPLE),
    };
    let result = admitted(phase, code);
    assert_eq!(result, expected);
    kani::cover!(code == Some(NORMAL_SUCCESS) && result);
    kani::cover!(code == Some(NORMAL_COUNTEREXAMPLE) && result);
    kani::cover!(code.is_none() && !result);
    kani::cover!(
        matches!(code, Some(value) if value != NORMAL_SUCCESS && value != NORMAL_COUNTEREXAMPLE)
            && !result
    );
}

#[kani::proof]
fn rejects_an_unrelated_exit_as_a_named_counterexample() {
    kani::assert(
        admitted(Phase::NamedNegative, Some(UNRELATED_FAILURE)),
        "NEGATIVE_PROOF_EXIT_CODE",
    );
}
