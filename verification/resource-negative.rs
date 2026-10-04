#[path = "../crates/domyjob-core/src/resource_budget.rs"]
mod resource_budget;

#[kani::proof]
fn rejects_three_jobs() {
    let invalid = resource_budget::Budget {
        concurrent: 3,
        high: 1,
        max: 2,
        swap: 0,
    };
    assert!(invalid.valid(), "NEGATIVE_RESOURCE_CONTROL");
}
