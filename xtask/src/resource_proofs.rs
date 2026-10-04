use serde::Deserialize;
use std::path::Path;

const HARNESSES: [(&str, u64); 3] = [
    (
        "resource_proofs::execution_records_preserve_command_outcomes",
        4,
    ),
    ("resource_proofs::resource_budget_is_bounded", 2),
    (
        "resource_proofs::job_transitions_preserve_queue_and_terminal_invariants",
        3,
    ),
];

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
    status: String,
    description: String,
    category: String,
}

fn read_report(path: &Path) -> Result<Report, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let bytes = std::io::Read::bytes(std::io::BufReader::new(file))
        .take(2_097_153)
        .collect::<std::io::Result<Vec<u8>>>()
        .map_err(|error| error.to_string())?;
    domyjob_core::ingress::json(&bytes, 2_097_152).map_err(|error| error.to_string())
}

fn check_positive(report: &Report) -> Result<(), String> {
    let summary = &report.verification_results.summary;
    if report.metadata.kani_version != "0.68.0"
        || summary.status != "completed"
        || summary.total_harnesses != 3
        || summary.executed != 3
        || summary.successful != 3
        || summary.failed != 0
        || report.property_details.len() != 3
        || report.verification_results.results.len() != 3
    {
        return Err(
            "resource proofs require exactly three completed Kani 0.68.0 harnesses".to_owned(),
        );
    }
    for (name, covers) in HARNESSES {
        let properties: Vec<_> = report
            .property_details
            .iter()
            .filter(|item| item.harness_id == name)
            .collect();
        let results: Vec<_> = report
            .verification_results
            .results
            .iter()
            .filter(|item| item.harness_id == name)
            .collect();
        let ([properties], [result]) = (properties.as_slice(), results.as_slice()) else {
            return Err(format!("missing or duplicate resource proof: {name}"));
        };
        let counts = &properties.property_details;
        if result.status != "Success"
            || result.checks.is_empty()
            || counts.failed != 0
            || counts.undetermined != 0
            || counts.solver_error != 0
            || counts.unsatisfiable != 0
            || counts.satisfied != covers
        {
            return Err(format!(
                "resource proof has incomplete checks or covers: {name}"
            ));
        }
    }
    Ok(())
}

fn check_negative(report: &Report) -> Result<(), String> {
    let summary = &report.verification_results.summary;
    let [result] = report.verification_results.results.as_slice() else {
        return Err("negative control must execute exactly one harness".to_owned());
    };
    let [properties] = report.property_details.as_slice() else {
        return Err("negative control needs one complete property summary".to_owned());
    };
    let counts = &properties.property_details;
    if report.metadata.kani_version == "0.68.0"
        && summary.status == "completed"
        && summary.total_harnesses == 1
        && summary.executed == 1
        && summary.failed == 1
        && summary.successful == 0
        && result.harness_id == "rejects_three_jobs"
        && result.status == "Failure"
        && properties.harness_id == "rejects_three_jobs"
        && counts.failed == 1
        && counts.undetermined == 0
        && counts.solver_error == 0
        && result.checks.iter().any(|check| {
            check.status == "Failure"
                && check.category == "assertion"
                && check.description.contains("NEGATIVE_RESOURCE_CONTROL")
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

fn command(root: &Path, target: &Path, report: &Path, cargo: bool) -> std::process::Command {
    let mut command = crate::raw::command("mise");
    command
        .current_dir(root)
        .args(["x", "cargo:kani-verifier@0.68.0", "--"]);
    if cargo {
        command.args(["cargo", "kani", "-p", "domyjob-core"]);
    } else {
        command
            .arg("kani")
            .arg(root.join("verification/resource-negative.rs"));
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
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let positive = directory.path().join("positive.json");
    let mut proof = command(root, &directory.path().join("core"), &positive, true);
    for (name, _) in HARNESSES {
        proof.args(["--harness", name]);
    }
    if !proof.status().map_err(|error| error.to_string())?.success() {
        return Err("source-bound resource proofs failed".to_owned());
    }
    check_positive(&read_report(&positive)?)?;
    let negative = directory.path().join("negative.json");
    let status = command(root, &directory.path().join("negative"), &negative, false)
        .args(["--harness", "rejects_three_jobs"])
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        return Err("the deliberately false resource assertion passed".to_owned());
    }
    check_negative(&read_report(&negative)?)
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
    use super::{Report, check_negative, check_positive};

    #[test]
    fn empty_or_tool_error_reports_cannot_pass_the_resource_gate() {
        let report: Report = domyjob_core::ingress::foreign_json(
            r#"{"metadata":{"kani_version":"0.68.0"},"property_details":[],"verification_results":{"summary":{"total_harnesses":0,"executed":0,"status":"completed","successful":0,"failed":0},"results":[]}}"#,
        ).unwrap();
        check_positive(&report).unwrap_err();
        check_negative(&report).unwrap_err();
    }
}
