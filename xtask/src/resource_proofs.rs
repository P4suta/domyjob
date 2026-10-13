use serde::Deserialize;
use std::path::Path;

pub const KANI_VERSION: &str = "0.68.0";
const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    pub source: Option<String>,
    pub name: String,
    pub covers: u64,
    pub contract: String,
    pub input_domain: String,
    pub bound: String,
    pub production: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Negative {
    pub source: Option<String>,
    pub harness: String,
    pub assertion: String,
}

#[derive(Debug, Deserialize)]
struct Report {
    metadata: Metadata,
    property_details: Vec<Properties>,
    verification_results: Results,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    kani_version: String,
}

#[derive(Debug, Deserialize)]
struct Properties {
    harness_id: String,
    property_details: Counts,
}

#[derive(Debug, Deserialize)]
struct Counts {
    failed: u64,
    undetermined: u64,
    solver_error: u64,
    satisfied: u64,
    unsatisfiable: u64,
}

#[derive(Debug, Deserialize)]
struct Results {
    summary: Summary,
    results: Vec<ProofResult>,
}

#[derive(Debug, Deserialize)]
struct Summary {
    total_harnesses: u64,
    executed: u64,
    status: String,
    successful: u64,
    failed: u64,
}

#[derive(Debug, Deserialize)]
struct ProofResult {
    harness_id: String,
    status: String,
    checks: Vec<Check>,
}

#[derive(Debug, Deserialize)]
struct Check {
    id: u64,
    status: String,
    description: String,
    category: String,
}

fn unique_checks(checks: &[Check]) -> bool {
    let identities = checks
        .iter()
        .map(|check| check.id)
        .collect::<std::collections::BTreeSet<_>>();
    identities.len() == checks.len()
}

fn read_report(path: &Path) -> Result<Report, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let bytes = std::io::Read::bytes(std::io::BufReader::new(file))
        .take(MAX_REPORT_BYTES.saturating_add(1))
        .collect::<std::io::Result<Vec<u8>>>()
        .map_err(|error| error.to_string())?;
    domyjob_core::ingress::json(&bytes, MAX_REPORT_BYTES).map_err(|error| error.to_string())
}

fn check_positive(report: &Report, harnesses: &[Harness]) -> Result<(), String> {
    let summary = &report.verification_results.summary;
    let count = u64::try_from(harnesses.len()).map_err(|error| error.to_string())?;
    if count == 0
        || report.metadata.kani_version != KANI_VERSION
        || summary.status != "completed"
        || summary.total_harnesses != count
        || summary.executed != count
        || summary.successful != count
        || summary.failed != 0
        || report.property_details.len() != harnesses.len()
        || report.verification_results.results.len() != harnesses.len()
    {
        return Err(
            "proof inventory requires every registered Kani harness to complete".to_owned(),
        );
    }
    for harness in harnesses {
        let name = &harness.name;
        let properties: Vec<_> = report
            .property_details
            .iter()
            .filter(|item| &item.harness_id == name)
            .collect();
        let results: Vec<_> = report
            .verification_results
            .results
            .iter()
            .filter(|item| &item.harness_id == name)
            .collect();
        let ([properties], [result]) = (properties.as_slice(), results.as_slice()) else {
            return Err(format!("missing or duplicate resource proof: {name}"));
        };
        let counts = &properties.property_details;
        let covers = u64::try_from(
            result
                .checks
                .iter()
                .filter(|check| check.category == "cover" && check.status == "Satisfied")
                .count(),
        )
        .map_err(|error| error.to_string())?;
        if result.status != "Success"
            || result.checks.is_empty()
            || !unique_checks(&result.checks)
            || covers != harness.covers
            || !result
                .checks
                .iter()
                .any(|check| check.category == "assertion" && check.status == "Success")
            || counts.failed != 0
            || counts.undetermined != 0
            || counts.solver_error != 0
            || counts.unsatisfiable != 0
            || counts.satisfied != harness.covers
            || result.checks.iter().any(|check| {
                !matches!(
                    check.status.as_str(),
                    "Success" | "Satisfied" | "Unreachable"
                )
            })
        {
            return Err(format!(
                "resource proof has incomplete checks or covers: {name}"
            ));
        }
    }
    Ok(())
}

fn check_negative(report: &Report, negative: &Negative) -> Result<(), String> {
    let summary = &report.verification_results.summary;
    let [result] = report.verification_results.results.as_slice() else {
        return Err("negative control must execute exactly one harness".to_owned());
    };
    let [properties] = report.property_details.as_slice() else {
        return Err("negative control needs one complete property summary".to_owned());
    };
    let counts = &properties.property_details;
    if report.metadata.kani_version == KANI_VERSION
        && summary.status == "completed"
        && summary.total_harnesses == 1
        && summary.executed == 1
        && summary.failed == 1
        && summary.successful == 0
        && result.harness_id == negative.harness
        && result.status == "Failure"
        && properties.harness_id == negative.harness
        && counts.failed == 1
        && counts.undetermined == 0
        && counts.solver_error == 0
        && unique_checks(&result.checks)
        && result
            .checks
            .iter()
            .filter(|check| check.status == "Failure")
            .count()
            == 1
        && result.checks.iter().all(|check| {
            matches!(
                check.status.as_str(),
                "Success" | "Failure" | "Unreachable" | "Satisfied"
            )
        })
        && result.checks.iter().any(|check| {
            check.status == "Failure"
                && check.category == "assertion"
                && check.description == negative.assertion
        })
    {
        Ok(())
    } else {
        Err(
            "negative control needs the expected assertion counterexample, not a tool error"
                .to_owned(),
        )
    }
}

fn command(
    root: &Path,
    target: &Path,
    report: &Path,
    source: Option<&str>,
) -> std::process::Command {
    let mut command = crate::raw::command("mise");
    command
        .current_dir(root)
        .args(["x", "cargo:kani-verifier@0.68.0", "--"]);
    if let Some(source) = source {
        command.arg("kani").arg(root.join(source));
    } else {
        command.args(["cargo", "kani", "-p", "domyjob-core"]);
    }
    command
        .args([
            "--exact",
            "--output-format=terse",
            "-Z",
            "unstable-options",
            "--harness-timeout=60s",
        ])
        .arg("--target-dir")
        .arg(target)
        .arg("--export-json")
        .arg(report)
        .env("CARGO_BUILD_JOBS", "2");
    command
}

pub fn run(root: &Path) -> Result<(), String> {
    crate::verification::run(root)
}

pub fn run_inventory(
    root: &Path,
    harnesses: &[Harness],
    negatives: &[Negative],
) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut groups = std::collections::BTreeMap::<_, Vec<Harness>>::new();
    for harness in harnesses {
        groups
            .entry(harness.source.clone())
            .or_default()
            .push(harness.clone());
    }
    if groups.is_empty() {
        return Err("source-bound proofs require a nonempty positive inventory".to_owned());
    }
    for (index, (source, group)) in groups.iter().enumerate() {
        let positive = directory.path().join(format!("positive-{index}.json"));
        let mut proof = command(
            root,
            &directory.path().join(format!("positive-{index}")),
            &positive,
            source.as_deref(),
        );
        for harness in group {
            proof.args(["--harness", &harness.name]);
        }
        if !proof.status().map_err(|error| error.to_string())?.success() {
            return Err("source-bound proofs failed".to_owned());
        }
        check_positive(&read_report(&positive)?, group)?;
    }
    for (index, probe) in negatives.iter().enumerate() {
        let negative = directory.path().join(format!("negative-{index}.json"));
        let status = command(
            root,
            &directory.path().join(format!("negative-{index}")),
            &negative,
            probe.source.as_deref(),
        )
        .args(["--harness", &probe.harness])
        .status()
        .map_err(|error| error.to_string())?;
        if status.success() {
            return Err(format!(
                "the deliberately false assertion passed: {}",
                probe.harness
            ));
        }
        check_negative(&read_report(&negative)?, probe)?;
    }
    Ok(())
}

pub fn native(root: &Path) -> Result<(), String> {
    if std::env::consts::OS != "linux" {
        return Ok(());
    }
    let status = crate::raw::command("mise")
        .current_dir(root)
        .args(["run", "resource-contracts"])
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("native Linux resource contracts failed".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{Harness, Negative, Report, check_negative, check_positive};

    fn harness(name: &str) -> Harness {
        Harness {
            source: None,
            name: name.to_owned(),
            covers: 1,
            contract: "fixture assertion".to_owned(),
            input_domain: "fixture".to_owned(),
            bound: "no loops".to_owned(),
            production: "fixture".to_owned(),
        }
    }

    #[test]
    fn empty_or_tool_error_reports_cannot_pass_the_resource_gate() {
        let report: Report = domyjob_core::ingress::foreign_json(
            r#"{"metadata":{"kani_version":"0.68.0"},"property_details":[],"verification_results":{"summary":{"total_harnesses":0,"executed":0,"status":"completed","successful":0,"failed":0},"results":[]}}"#,
        ).unwrap();
        check_positive(&report, &[harness("missing")]).unwrap_err();
        check_negative(
            &report,
            &Negative {
                source: None,
                harness: "missing".to_owned(),
                assertion: "EXPECTED".to_owned(),
            },
        )
        .unwrap_err();
    }

    fn positive() -> (Report, Vec<Harness>) {
        let report = domyjob_core::ingress::foreign_json(r#"{"metadata":{"kani_version":"0.68.0"},"property_details":[{"harness_id":"contract","property_details":{"failed":0,"undetermined":0,"solver_error":0,"satisfied":1,"unsatisfiable":0}}],"verification_results":{"summary":{"total_harnesses":1,"executed":1,"status":"completed","successful":1,"failed":0},"results":[{"harness_id":"contract","status":"Success","checks":[{"id":1,"status":"Success","description":"contract assertion","category":"assertion"},{"id":2,"status":"Satisfied","description":"reachable branch","category":"cover"}]}]}}"#).unwrap();
        (report, vec![harness("contract")])
    }

    fn negative_fixture() -> (serde_json::Value, Negative) {
        let report = serde_json::json!({
            "metadata":{"kani_version":super::KANI_VERSION},
            "property_details":[{"harness_id":"contract","property_details":{
                "failed":1,"undetermined":0,"solver_error":0,"satisfied":0,"unsatisfiable":0
            }}],
            "verification_results":{
                "summary":{"total_harnesses":1,"executed":1,"status":"completed","successful":0,"failed":1},
                "results":[{"harness_id":"contract","status":"Failure","checks":[{
                    "id":1,"status":"Failure","description":"EXPECTED_CONTROL","category":"assertion"
                }]}]
            }
        });
        (
            report,
            Negative {
                source: None,
                harness: "contract".to_owned(),
                assertion: "EXPECTED_CONTROL".to_owned(),
            },
        )
    }

    #[test]
    fn complete_proofs_and_the_exact_named_counterexample_are_admitted() {
        let (proved, harnesses) = positive();
        check_positive(&proved, &harnesses).unwrap();
        let (value, negative) = negative_fixture();
        let report = domyjob_core::ingress::value(value).unwrap();
        check_negative(&report, &negative).unwrap();
    }

    fn summary_faults() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("/metadata/kani_version", serde_json::json!("unpinned")),
            (
                "/verification_results/summary/status",
                serde_json::json!("skipped"),
            ),
            (
                "/verification_results/summary/total_harnesses",
                serde_json::json!(0),
            ),
            (
                "/verification_results/summary/executed",
                serde_json::json!(0),
            ),
            ("/verification_results/summary/failed", serde_json::json!(0)),
            (
                "/verification_results/summary/successful",
                serde_json::json!(1),
            ),
            (
                "/verification_results/results/0/harness_id",
                serde_json::json!("foreign"),
            ),
            (
                "/verification_results/results/0/status",
                serde_json::json!("Success"),
            ),
            (
                "/property_details/0/harness_id",
                serde_json::json!("foreign"),
            ),
            (
                "/property_details/0/property_details/failed",
                serde_json::json!(0),
            ),
            (
                "/property_details/0/property_details/undetermined",
                serde_json::json!(1),
            ),
            (
                "/property_details/0/property_details/solver_error",
                serde_json::json!(1),
            ),
        ]
    }

    fn assertion_faults() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            (
                "/verification_results/results/0/checks/0/status",
                serde_json::json!("SolverError"),
            ),
            (
                "/verification_results/results/0/checks/0/category",
                serde_json::json!("overflow"),
            ),
            (
                "/verification_results/results/0/checks/0/description",
                serde_json::json!("NOT_EXPECTED_CONTROL"),
            ),
            (
                "/verification_results/results/0/checks",
                serde_json::json!([]),
            ),
        ]
    }

    #[test]
    fn each_incomplete_counterexample_field_is_refused_with_a_diagnostic() {
        for (pointer, fault) in summary_faults().into_iter().chain(assertion_faults()) {
            let (mut value, negative) = negative_fixture();
            value.pointer_mut(pointer).unwrap().clone_from(&fault);
            let report = domyjob_core::ingress::value(value).unwrap();
            let error = check_negative(&report, &negative).unwrap_err();
            assert_ne!(error.trim(), "", "missing refusal diagnostic for {pointer}");
        }
    }

    #[test]
    fn missing_or_multiple_counterexample_records_are_refused_with_a_diagnostic() {
        for pointer in ["/property_details", "/verification_results/results"] {
            let (template, _) = negative_fixture();
            let first = template
                .pointer(pointer)
                .unwrap()
                .as_array()
                .unwrap()
                .first()
                .unwrap();
            for fault in [serde_json::json!([]), serde_json::json!([first, first])] {
                let (mut value, negative) = negative_fixture();
                value.pointer_mut(pointer).unwrap().clone_from(&fault);
                let report = domyjob_core::ingress::value(value).unwrap();
                let error = check_negative(&report, &negative).unwrap_err();
                assert_ne!(error.trim(), "", "missing record diagnostic for {pointer}");
            }
        }
    }

    #[test]
    fn a_successful_named_assertion_cannot_hide_an_unrelated_failure() {
        let (mut value, negative) = negative_fixture();
        value.pointer_mut("/verification_results/results/0/checks").unwrap().clone_from(&serde_json::json!([
            {"id":1,"status":"Failure","description":"unrelated overflow","category":"overflow"},
            {"id":2,"status":"Success","description":"EXPECTED_CONTROL","category":"assertion"}
        ]));
        let report = domyjob_core::ingress::value(value).unwrap();
        let error = check_negative(&report, &negative).unwrap_err();
        assert_ne!(error.trim(), "");
    }

    #[test]
    fn duplicate_or_missing_cover_checks_cannot_satisfy_the_summary() {
        let (mut missing, harnesses) = positive();
        missing
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
            .pop();
        check_positive(&missing, &harnesses).unwrap_err();
        let (mut duplicate, _) = positive();
        duplicate
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
            .push(super::Check {
                id: 2,
                status: "Satisfied".to_owned(),
                description: "reachable branch".to_owned(),
                category: "cover".to_owned(),
            });
        duplicate
            .property_details
            .first_mut()
            .unwrap()
            .property_details
            .satisfied = 2;
        let mut expected = harnesses;
        expected.first_mut().unwrap().covers = 2;
        check_positive(&duplicate, &expected).unwrap_err();
    }

    #[test]
    fn reachable_covers_cannot_substitute_for_a_proved_assertion() {
        let (mut report, harnesses) = positive();
        for check in &mut report
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
        {
            check.category = "cover".to_owned();
        }
        check_positive(&report, &harnesses).unwrap_err();
    }

    #[test]
    fn positive_reports_require_the_exact_harness_and_reachable_covers() {
        let (mut uncovered, harnesses) = positive();
        check_positive(&uncovered, &harnesses).unwrap();
        uncovered
            .property_details
            .first_mut()
            .unwrap()
            .property_details
            .satisfied = 0;
        check_positive(&uncovered, &harnesses).unwrap_err();
        let (mut stale, _) = positive();
        stale
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .harness_id = "stale".to_owned();
        check_positive(&stale, &harnesses).unwrap_err();
        let (mut failed, _) = positive();
        failed
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
            .first_mut()
            .unwrap()
            .status = "Failure".to_owned();
        check_positive(&failed, &harnesses).unwrap_err();
        let (mut skipped, _) = positive();
        skipped.verification_results.summary.executed = 0;
        check_positive(&skipped, &harnesses).unwrap_err();
    }

    #[test]
    fn a_solver_failure_cannot_substitute_for_a_negative_assertion() {
        let (mut report, _) = positive();
        let negative = Negative {
            source: None,
            harness: "contract".to_owned(),
            assertion: "EXPECTED_CONTROL".to_owned(),
        };
        report.verification_results.summary.failed = 1;
        report.verification_results.summary.successful = 0;
        report
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .status = "Failure".to_owned();
        report
            .property_details
            .first_mut()
            .unwrap()
            .property_details
            .failed = 1;
        report
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
            .first_mut()
            .unwrap()
            .status = "Failure".to_owned();
        report
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
            .first_mut()
            .unwrap()
            .description = "EXPECTED_CONTROL".to_owned();
        check_negative(&report, &negative).unwrap();
        report
            .property_details
            .first_mut()
            .unwrap()
            .property_details
            .solver_error = 1;
        check_negative(&report, &negative).unwrap_err();
        report
            .property_details
            .first_mut()
            .unwrap()
            .property_details
            .solver_error = 0;
        report
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
            .first_mut()
            .unwrap()
            .description = "NOT_EXPECTED_CONTROL".to_owned();
        check_negative(&report, &negative).unwrap_err();
        report
            .verification_results
            .results
            .first_mut()
            .unwrap()
            .checks
            .first_mut()
            .unwrap()
            .description = "unrelated assertion".to_owned();
        check_negative(&report, &negative).unwrap_err();
    }
}
