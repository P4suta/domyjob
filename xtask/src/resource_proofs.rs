use serde::Deserialize;
use std::path::Path;

mod exit_policy;
pub(crate) mod process;

use crate::proof_tools::NativeDefaultBinding;
use exit_policy::Phase;
use process::check_exit;

pub const KANI_VERSION: &str = "0.68.0";
const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
const STANDALONE_RUSTFLAGS: &str = "--edition=2024";

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

#[derive(Clone, Debug, Deserialize)]
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

fn report_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let bytes = std::io::Read::bytes(std::io::BufReader::new(file))
        .take(MAX_REPORT_BYTES.saturating_add(1))
        .collect::<std::io::Result<Vec<u8>>>()
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_REPORT_BYTES {
        Err("Kani report exceeds its unchanged byte bound".to_owned())
    } else {
        Ok(bytes)
    }
}

#[derive(Debug, Deserialize)]
struct BackendReport {
    metadata: BackendMetadata,
    harness_metadata: Vec<HarnessMetadata>,
    cbmc: Vec<BackendObservation>,
    error_details: Vec<ErrorObservation>,
    property_details: Vec<NativeProperties>,
}

#[derive(Debug, Deserialize)]
struct BackendMetadata {
    kani_version: String,
    target: String,
}

#[derive(Debug, Deserialize)]
struct HarnessMetadata {
    pretty_name: String,
    mangled_name: String,
}

#[derive(Debug, Deserialize)]
struct BackendObservation {
    harness_id: String,
    cbmc_metadata: CbmcMetadata,
    configuration: BackendConfiguration,
}

#[derive(Debug, Deserialize)]
struct CbmcMetadata {
    version: String,
    os_info: String,
}

#[derive(Debug, Deserialize)]
struct BackendConfiguration {
    solver: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ErrorObservation {
    Clean(CleanObservation),
    Assertion(AssertionObservation),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanObservation {
    harness_id: String,
    has_errors: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssertionObservation {
    harness_id: String,
    has_errors: bool,
    error_type: AssertionError,
    exit_status: PropertyExit,
    failed_properties_type: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AssertionError {
    AssertionFailure,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PropertyExit {
    PropertiesFailed,
}

impl ErrorObservation {
    fn name(&self) -> &str {
        match self {
            Self::Clean(observed) => &observed.harness_id,
            Self::Assertion(observed) => &observed.harness_id,
        }
    }

    const fn admits(&self, phase: Phase) -> bool {
        match (phase, self) {
            (Phase::Positive, Self::Clean(observed)) => !observed.has_errors,
            (Phase::NamedNegative, Self::Assertion(observed)) => {
                observed.has_errors
                    && matches!(observed.error_type, AssertionError::AssertionFailure)
                    && matches!(observed.exit_status, PropertyExit::PropertiesFailed)
                    && !observed.failed_properties_type.is_empty()
            }
            (Phase::Positive, Self::Assertion(_)) | (Phase::NamedNegative, Self::Clean(_)) => false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct NativeProperties {
    harness_id: String,
    property_details: NativeCounts,
}

#[derive(Debug, Deserialize)]
struct NativeCounts {
    total_properties: u64,
    passed: u64,
    failed: u64,
    unreachable: u64,
    undetermined: u64,
    solver_error: u64,
    satisfied: u64,
    unsatisfiable: u64,
    covered: u64,
    uncovered: u64,
}

fn names<'a>(
    values: impl Iterator<Item = &'a str>,
) -> Result<std::collections::BTreeSet<&'a str>, String> {
    let mut names = std::collections::BTreeSet::new();
    for name in values {
        if name.is_empty() || !names.insert(name) {
            return Err("empty or duplicate backend observation identity".to_owned());
        }
    }
    if names.is_empty() {
        Err("backend observations must be nonempty".to_owned())
    } else {
        Ok(names)
    }
}

impl BackendReport {
    fn observation_names(&self, report: &Report) -> Result<(), String> {
        let expected = names(
            report
                .verification_results
                .results
                .iter()
                .map(|result| result.harness_id.as_str()),
        )?;
        if names(
            self.harness_metadata
                .iter()
                .map(|item| item.pretty_name.as_str()),
        )? != expected
            || names(self.cbmc.iter().map(|item| item.harness_id.as_str()))? != expected
            || names(self.error_details.iter().map(ErrorObservation::name))? != expected
            || names(
                self.property_details
                    .iter()
                    .map(|item| item.harness_id.as_str()),
            )? != expected
            || names(
                self.harness_metadata
                    .iter()
                    .map(|item| item.mangled_name.as_str()),
            )?
            .len()
                != expected.len()
        {
            return Err("missing or conflicting exact backend harness identity".to_owned());
        }
        Ok(())
    }

    fn properties(&self, report: &Report) -> Result<(), String> {
        for result in &report.verification_results.results {
            let properties = self
                .property_details
                .iter()
                .find(|item| item.harness_id == result.harness_id)
                .ok_or_else(|| "missing native property counts".to_owned())?;
            properties.property_details.validate(result)?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct DefaultReportOverlay {
    tools: DefaultReportTools,
    cbmc: Vec<DefaultConfiguration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultReportTools {
    kani: String,
    rustc: String,
    cbmc: String,
    goto_cc: String,
    goto_instrument: String,
    solvers: Vec<DefaultBuiltinSolver>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultBuiltinSolver {
    name: String,
    version: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct DefaultConfiguration {
    harness_id: String,
    configuration: DefaultObjectBits,
}

#[derive(Debug, Deserialize)]
struct DefaultObjectBits {
    object_bits: u32,
}

fn default_metadata(
    bytes: &[u8],
    report: &Report,
    phase: Phase,
    binding: &NativeDefaultBinding,
) -> Result<(), String> {
    let versions = binding.versions();
    DefaultReportExpectation {
        target: binding.target()?,
        compiler: &versions.compiler,
        cbmc: &versions.cbmc,
        goto_cc: &versions.goto_cc,
        goto_instrument: &versions.goto_instrument,
    }
    .validate(bytes, report, phase)
}

struct DefaultReportExpectation<'a> {
    target: crate::proof_tools::native_default::Target,
    compiler: &'a str,
    cbmc: &'a str,
    goto_cc: &'a str,
    goto_instrument: &'a str,
}

impl DefaultReportExpectation<'_> {
    fn validate(&self, bytes: &[u8], report: &Report, phase: Phase) -> Result<(), String> {
        let common: BackendReport = domyjob_core::ingress::json(bytes, MAX_REPORT_BYTES)
            .map_err(|error| error.to_string())?;
        let overlay: DefaultReportOverlay = domyjob_core::ingress::json(bytes, MAX_REPORT_BYTES)
            .map_err(|error| error.to_string())?;
        common.observation_names(report)?;
        common.properties(report)?;
        let target = self.target;
        let [builtin] = overlay.tools.solvers.as_slice() else {
            return Err("native-default report requires exactly one builtin solver".to_owned());
        };
        if common.metadata.kani_version != KANI_VERSION
            || common.metadata.target != target.triple()
            || overlay.tools.kani != KANI_VERSION
            || overlay.tools.rustc != self.compiler
            || overlay.tools.cbmc != self.cbmc
            || overlay.tools.goto_cc != self.goto_cc
            || overlay.tools.goto_instrument != self.goto_instrument
            || builtin.name != "cadical"
            || !builtin.version.is_null()
        {
            return Err(
                "native-default report conflicts with its observed target or tools".to_owned(),
            );
        }
        if names(overlay.cbmc.iter().map(|item| item.harness_id.as_str()))?
            != names(common.cbmc.iter().map(|item| item.harness_id.as_str()))?
        {
            return Err(
                "native-default configuration changed its exact harness identities".to_owned(),
            );
        }
        for observation in &common.cbmc {
            let configuration = overlay
                .cbmc
                .iter()
                .find(|item| item.harness_id == observation.harness_id)
                .ok_or_else(|| "missing native-default object-bit observation".to_owned())?;
            let solver = match observation.configuration.solver.as_str() {
                "cadical" => crate::proof_tools::native_default::Solver::Cadical,
                "" => crate::proof_tools::native_default::Solver::Missing,
                _ => crate::proof_tools::native_default::Solver::Other,
            };
            if observation.cbmc_metadata.version != "6.11.0"
                || observation.cbmc_metadata.os_info != target.cbmc_os()
                || !crate::proof_tools::native_default::configuration(
                    solver,
                    configuration.configuration.object_bits,
                    builtin.version.is_null(),
                )
            {
                return Err("conflicting native-default CBMC configuration".to_owned());
            }
        }
        if common.error_details.iter().any(|item| {
            !item.admits(phase)
                || matches!(item, ErrorObservation::Assertion(observed) if observed.failed_properties_type != "PanicsOnly")
        }) {
            return Err("conflicting native-default phase observation".to_owned());
        }
        Ok(())
    }
}

impl NativeCounts {
    fn validate(&self, result: &ProofResult) -> Result<(), String> {
        let count = |status| {
            u64::try_from(
                result
                    .checks
                    .iter()
                    .filter(|check| check.status == status)
                    .count(),
            )
            .map_err(|error| error.to_string())
        };
        if self.total_properties
            == u64::try_from(result.checks.len()).map_err(|error| error.to_string())?
            && self.passed == count("Success")?
            && self.failed == count("Failure")?
            && self.unreachable == count("Unreachable")?
            && self.satisfied == count("Satisfied")?
            && self.undetermined == 0
            && self.solver_error == 0
            && self.unsatisfiable == 0
            && self.covered == 0
            && self.uncovered == 0
        {
            Ok(())
        } else {
            Err("native property totals contradict exact check statuses".to_owned())
        }
    }
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
    mut command: std::process::Command,
    target: &Path,
    report: &Path,
    (source, phase): (Option<&str>, Phase),
) -> std::process::Command {
    if source.is_some() {
        command.env("RUSTFLAGS", STANDALONE_RUSTFLAGS);
    }
    command
        .args([
            "--exact",
            "--output-format=terse",
            "--jobs=1",
            "-Z",
            "unstable-options",
        ])
        .arg("--target-dir")
        .arg(target)
        .arg("--export-json")
        .arg(report)
        .env("CARGO_BUILD_JOBS", "2");
    if phase == Phase::Positive {
        command.arg("--fail-fast");
    }
    crate::proof_tools::isolate_native_default(&mut command);
    command
}

pub fn run(root: &Path) -> Result<(), String> {
    crate::verification::dispatch(root, &[])
}

pub fn run_inventory(
    root: &Path,
    harnesses: &[Harness],
    negatives: &[Negative],
) -> Result<(), String> {
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
    for (source, group) in &groups {
        let names = group
            .iter()
            .map(|harness| harness.name.as_str())
            .collect::<Vec<_>>();
        attempt(
            root,
            ProofRun {
                source: source.as_deref(),
                phase: Phase::Positive,
                names: &names,
            },
            |report| check_positive(report, group),
        )?;
    }
    for probe in negatives {
        attempt(
            root,
            ProofRun {
                source: probe.source.as_deref(),
                phase: Phase::NamedNegative,
                names: &[probe.harness.as_str()],
            },
            |report| check_negative(report, probe),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ProofRun<'a> {
    source: Option<&'a str>,
    phase: Phase,
    names: &'a [&'a str],
}

fn checked_binding(
    primary: Result<(), String>,
    after: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let after = after();
    match (primary, after) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(tool)) => Err(format!("native-default validation: {tool}")),
        (Err(primary), Err(tool)) => Err(format!("{primary}; native-default validation: {tool}")),
    }
}

fn attempt(
    root: &Path,
    run: ProofRun<'_>,
    validate: impl FnOnce(&Report) -> Result<(), String>,
) -> Result<(), String> {
    let directory =
        crate::proof_tools::evidence(root, crate::proof_tools::EvidenceKind::ProofAttempt)?;
    eprintln!("proof attempt evidence: {}", directory.display());
    let before = NativeDefaultBinding::capture(root, &directory);
    let primary = match before.as_ref() {
        Ok(binding) => execute(root, run, (&directory, binding), validate),
        Err(error) => Err(error.clone()),
    };
    checked_binding(primary, || match before {
        Ok(binding) => binding.recheck(root, &directory),
        Err(_) => match NativeDefaultBinding::capture(root, &directory) {
            Ok(_) => Err("native-default lacked an admitted before-observation".to_owned()),
            Err(error) => Err(error),
        },
    })
    .map_err(|error| format!("{error}; evidence {}", directory.display()))
}

fn execute(
    root: &Path,
    run: ProofRun<'_>,
    observation: (&Path, &NativeDefaultBinding),
    validate: impl FnOnce(&Report) -> Result<(), String>,
) -> Result<(), String> {
    let (directory, binding) = observation;
    let report_path = directory.join("results.json");
    let mut proof = command(
        binding.command(root, run.source),
        &directory.join("target"),
        &report_path,
        (run.source, run.phase),
    );
    for name in run.names {
        proof.args(["--harness", name]);
    }
    check_exit(
        proof.status().map_err(|error| error.to_string())?,
        run.phase,
    )?;
    admit_report(
        &report_bytes(&report_path)?,
        run.phase,
        Some(binding),
        validate,
    )
}

fn admit_report(
    bytes: &[u8],
    phase: Phase,
    binding: Option<&NativeDefaultBinding>,
    validate: impl FnOnce(&Report) -> Result<(), String>,
) -> Result<(), String> {
    let report =
        domyjob_core::ingress::json(bytes, MAX_REPORT_BYTES).map_err(|error| error.to_string())?;
    validate(&report)?;
    let binding =
        binding.ok_or_else(|| "native-default report lacks actual tool binding".to_owned())?;
    default_metadata(bytes, &report, phase, binding)
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

    #[test]
    fn pure_exit_policy_requires_exact_normal_codes() {
        let cases = [
            (None, false, false),
            (Some(0), true, false),
            (Some(1), false, true),
            (Some(2), false, false),
            (Some(i32::MIN), false, false),
            (Some(i32::MAX), false, false),
        ];
        for (code, positive, negative) in cases {
            assert_eq!(
                super::exit_policy::admitted(super::Phase::Positive, code),
                positive
            );
            assert_eq!(
                super::exit_policy::admitted(super::Phase::NamedNegative, code),
                negative
            );
        }
    }

    #[test]
    fn actual_proof_command_limits_jobs_without_a_machine_speed_deadline() {
        let root = std::path::Path::new("root");
        for phase in [super::Phase::Positive, super::Phase::NamedNegative] {
            let command = super::command(
                crate::raw::command("admitted proof"),
                root,
                root,
                (None, phase),
            );
            let arguments = command
                .get_args()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>();
            assert_eq!(
                arguments
                    .iter()
                    .filter(|value| *value == "--jobs=1")
                    .count(),
                1
            );
            assert_eq!(
                arguments.iter().any(|value| value == "--fail-fast"),
                phase == super::Phase::Positive
            );
            assert!(
                !arguments
                    .iter()
                    .any(|value| value.starts_with("--harness-timeout"))
            );
            assert!(!arguments.iter().any(|value| value.starts_with("--solver")));
        }
    }

    #[test]
    fn standalone_commands_select_the_workspace_edition_and_isolate_loader_paths() {
        let root = std::path::Path::new("root");
        for source in [None, Some("verification/native-default-tool-binding.rs")] {
            let command = super::command(
                crate::raw::command("admitted proof"),
                root,
                root,
                (source, super::Phase::Positive),
            );
            let variables = command
                .get_envs()
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                variables.get(std::ffi::OsStr::new("RUSTFLAGS")),
                source
                    .map(|_| Some(std::ffi::OsStr::new(super::STANDALONE_RUSTFLAGS)))
                    .as_ref()
            );
            for removed in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
                assert_eq!(variables.get(std::ffi::OsStr::new(removed)), Some(&None));
            }
        }
    }

    #[test]
    fn tool_rechecks_run_after_every_primary_outcome_and_preserve_both_errors() {
        for primary in [Ok(()), Err("primary failure".to_owned())] {
            for after in [Ok(()), Err("changed tools".to_owned())] {
                let called = std::cell::Cell::new(false);
                let result = super::checked_binding(primary.clone(), || {
                    called.set(true);
                    after.clone()
                });
                assert!(called.get());
                assert_eq!(result.is_ok(), primary.is_ok() && after.is_ok());
                if let Err(error) = &primary {
                    assert!(result.as_ref().unwrap_err().contains(error));
                }
                if let Err(error) = after {
                    assert!(result.as_ref().unwrap_err().contains(&error));
                }
            }
        }
    }

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

    fn default_metadata_fixture() -> (serde_json::Value, super::DefaultReportExpectation<'static>) {
        let value = serde_json::json!({
            "metadata":{"kani_version":super::KANI_VERSION,"target":"x86_64-unknown-linux-gnu"},
            "tools":{"kani":super::KANI_VERSION,"cbmc":"6.11.0 (cbmc-6.11.0)",
                "rustc":crate::proof_tools::DEFAULT_COMPILER_VERSION,
                "goto_cc":"observed goto-cc","goto_instrument":"6.11.0 (cbmc-6.11.0)",
                "solvers":[{"name":"cadical","version":null}]},
            "harness_metadata":[{"pretty_name":"contract","mangled_name":"unique_contract_symbol"}],
            "cbmc":[{"harness_id":"contract","cbmc_metadata":{"version":"6.11.0","os_info":"x86_64 linux unix"},
                "configuration":{"solver":"cadical","object_bits":16}}],
            "error_details":[{"harness_id":"contract","has_errors":false}],
            "property_details":[{"harness_id":"contract","property_details":{
                "total_properties":2,"passed":1,"failed":0,"unreachable":0,"undetermined":0,
                "solver_error":0,"satisfied":1,"unsatisfiable":0,"covered":0,"uncovered":0}}]
        });
        let expected = super::DefaultReportExpectation {
            target: crate::proof_tools::native_default::Target::LinuxAmd64,
            compiler: crate::proof_tools::DEFAULT_COMPILER_VERSION,
            cbmc: "6.11.0 (cbmc-6.11.0)",
            goto_cc: "observed goto-cc",
            goto_instrument: "6.11.0 (cbmc-6.11.0)",
        };
        (value, expected)
    }

    fn assert_metadata_substitutions_refused(
        value: &serde_json::Value,
        context: (&super::DefaultReportExpectation<'_>, &Report, super::Phase),
        substitutions: impl IntoIterator<Item = (&'static str, serde_json::Value)>,
    ) {
        let (expected, report, phase) = context;
        for (pointer, replacement) in substitutions {
            let mut substituted = value.clone();
            *substituted.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                expected
                    .validate(&serde_json::to_vec(&substituted).unwrap(), report, phase)
                    .is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn native_default_report_requires_an_actual_tool_binding() {
        let (mut value, _) = default_metadata_fixture();
        value.as_object_mut().unwrap().insert("verification_results".to_owned(), serde_json::json!({
            "summary":{"total_harnesses":1,"executed":1,"status":"completed","successful":1,"failed":0},
            "results":[{"harness_id":"contract","status":"Success","checks":[
                {"id":1,"status":"Success","description":"contract assertion","category":"assertion"},
                {"id":2,"status":"Satisfied","description":"reachable branch","category":"cover"}
            ]}]
        }));
        let (_, harnesses) = positive();
        let error = super::admit_report(
            &serde_json::to_vec(&value).unwrap(),
            super::Phase::Positive,
            None,
            |report| check_positive(report, &harnesses),
        )
        .unwrap_err();
        assert_eq!(error, "native-default report lacks actual tool binding");
    }

    #[test]
    fn native_default_metadata_requires_exact_tools_and_configuration() {
        let (value, expected) = default_metadata_fixture();
        let (report, _) = positive();
        let valid = serde_json::to_vec(&value).unwrap();
        expected
            .validate(&valid, &report, super::Phase::Positive)
            .unwrap();
        expected
            .validate(&valid, &report, super::Phase::NamedNegative)
            .unwrap_err();
        assert_metadata_substitutions_refused(
            &value,
            (&expected, &report, super::Phase::Positive),
            [
                (
                    "/metadata/target",
                    serde_json::json!("aarch64-apple-darwin"),
                ),
                ("/metadata/kani_version", serde_json::json!("0.67.0")),
                ("/tools/kani", serde_json::json!("0.67.0")),
                ("/tools/rustc", serde_json::json!("foreign compiler")),
                ("/tools/cbmc", serde_json::json!("foreign CBMC")),
                ("/tools/goto_cc", serde_json::json!("foreign goto-cc")),
                (
                    "/tools/goto_instrument",
                    serde_json::json!("foreign goto-instrument"),
                ),
                ("/tools/solvers/0/name", serde_json::json!("z3")),
                ("/tools/solvers/0/version", serde_json::json!("unknown")),
                ("/cbmc/0/configuration/solver", serde_json::json!("z3")),
                (
                    "/cbmc/0/configuration/object_bits",
                    serde_json::json!(u32::MAX),
                ),
                (
                    "/cbmc/0/cbmc_metadata/os_info",
                    serde_json::json!("x86_64 macos unix"),
                ),
                ("/cbmc/0/cbmc_metadata/version", serde_json::json!("6.10.0")),
                ("/cbmc/0/harness_id", serde_json::json!("foreign")),
                (
                    "/harness_metadata/0/pretty_name",
                    serde_json::json!("foreign"),
                ),
                ("/harness_metadata/0/mangled_name", serde_json::json!("")),
                ("/error_details/0/has_errors", serde_json::json!(true)),
            ],
        );
    }

    #[test]
    fn native_default_property_counts_match_every_reported_status() {
        let (value, expected) = default_metadata_fixture();
        let (report, _) = positive();
        assert_metadata_substitutions_refused(
            &value,
            (&expected, &report, super::Phase::Positive),
            [
                (
                    "/property_details/0/property_details/total_properties",
                    serde_json::json!(u64::MAX),
                ),
                (
                    "/property_details/0/property_details/passed",
                    serde_json::json!(0),
                ),
                (
                    "/property_details/0/property_details/failed",
                    serde_json::json!(1),
                ),
                (
                    "/property_details/0/property_details/unreachable",
                    serde_json::json!(1),
                ),
                (
                    "/property_details/0/property_details/undetermined",
                    serde_json::json!(1),
                ),
                (
                    "/property_details/0/property_details/solver_error",
                    serde_json::json!(1),
                ),
                (
                    "/property_details/0/property_details/satisfied",
                    serde_json::json!(0),
                ),
                (
                    "/property_details/0/property_details/unsatisfiable",
                    serde_json::json!(1),
                ),
                (
                    "/property_details/0/property_details/covered",
                    serde_json::json!(1),
                ),
                (
                    "/property_details/0/property_details/uncovered",
                    serde_json::json!(1),
                ),
            ],
        );
    }

    #[test]
    fn native_default_metadata_refuses_missing_keys_and_duplicate_observations() {
        let (value, expected) = default_metadata_fixture();
        let (report, _) = positive();
        for (parent, field) in [
            ("/tools/solvers/0", "version"),
            ("/tools", "goto_cc"),
            ("/cbmc/0/configuration", "object_bits"),
        ] {
            let mut missing = value.clone();
            missing
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                expected
                    .validate(
                        &serde_json::to_vec(&missing).unwrap(),
                        &report,
                        super::Phase::Positive
                    )
                    .is_err(),
                "{field}"
            );
        }
        for pointer in [
            "/tools/solvers",
            "/cbmc",
            "/harness_metadata",
            "/error_details",
            "/property_details",
        ] {
            let mut duplicate = value.clone();
            let rows = duplicate
                .pointer_mut(pointer)
                .unwrap()
                .as_array_mut()
                .unwrap();
            rows.push(rows.first().unwrap().clone());
            assert!(
                expected
                    .validate(
                        &serde_json::to_vec(&duplicate).unwrap(),
                        &report,
                        super::Phase::Positive
                    )
                    .is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn native_default_negative_requires_the_exact_panics_only_phase() {
        let (mut value, expected) = default_metadata_fixture();
        let (negative, _) = negative_fixture();
        let report: Report = domyjob_core::ingress::value(negative).unwrap();
        *value
            .pointer_mut("/property_details/0/property_details")
            .unwrap() = serde_json::json!({
            "total_properties":1,"passed":0,"failed":1,"unreachable":0,"undetermined":0,
            "solver_error":0,"satisfied":0,"unsatisfiable":0,"covered":0,"uncovered":0
        });
        *value.pointer_mut("/error_details/0").unwrap() = serde_json::json!({
            "harness_id":"contract","has_errors":true,"error_type":"assertion_failure",
            "exit_status":"properties_failed","failed_properties_type":"PanicsOnly"
        });
        expected
            .validate(
                &serde_json::to_vec(&value).unwrap(),
                &report,
                super::Phase::NamedNegative,
            )
            .unwrap();
        assert_metadata_substitutions_refused(
            &value,
            (&expected, &report, super::Phase::NamedNegative),
            [
                ("/error_details/0/has_errors", serde_json::json!(false)),
                (
                    "/error_details/0/error_type",
                    serde_json::json!("solver_error"),
                ),
                (
                    "/error_details/0/exit_status",
                    serde_json::json!("tool_failed"),
                ),
                (
                    "/error_details/0/failed_properties_type",
                    serde_json::json!("Other"),
                ),
                (
                    "/error_details/0/failed_properties_type",
                    serde_json::json!(""),
                ),
            ],
        );
    }

    fn component_positive(harness: &Harness) -> Report {
        const FIRST_COVER_CHECK: u64 = 2;
        let (mut report, _) = positive();
        let properties = report.property_details.first_mut().unwrap();
        properties.harness_id.clone_from(&harness.name);
        properties.property_details.satisfied = harness.covers;
        let result = report.verification_results.results.first_mut().unwrap();
        result.harness_id.clone_from(&harness.name);
        result.checks.retain(|check| check.category == "assertion");
        result
            .checks
            .extend((0..harness.covers).map(|cover| super::Check {
                id: FIRST_COVER_CHECK.checked_add(cover).unwrap(),
                status: "Satisfied".to_owned(),
                description: "reachable component branch".to_owned(),
                category: "cover".to_owned(),
            }));
        report
    }

    #[test]
    fn component_reports_require_each_registered_name_and_every_cover() {
        let manifest: serde_json::Value = domyjob_core::ingress::foreign_json(include_str!(
            "../../verification/obligations.json"
        ))
        .unwrap();
        let harnesses: Vec<Harness> =
            domyjob_core::ingress::value(manifest.get("harnesses").unwrap().clone()).unwrap();
        for (name, covers) in [
            (
                "chat::ledger::parent_identity::proofs::canonical_parent_identity_generator_preserves_every_origin_byte",
                1,
            ),
            (
                "chat::ledger::parent_identity::proofs::present_parent_identity_matches_exact_origin_and_sequence",
                4,
            ),
            (
                "chat::ledger::parent_identity::proofs::absent_parent_identity_accepts_every_request",
                2,
            ),
        ] {
            let harness = harnesses
                .iter()
                .find(|harness| harness.name == name)
                .unwrap();
            assert_eq!(harness.covers, covers);
            let selected = std::slice::from_ref(harness);
            check_positive(&component_positive(harness), selected).unwrap();

            let mut missing = component_positive(harness);
            missing
                .verification_results
                .results
                .first_mut()
                .unwrap()
                .checks
                .pop()
                .unwrap();
            check_positive(&missing, selected).unwrap_err();

            let mut unsatisfied = component_positive(harness);
            unsatisfied
                .verification_results
                .results
                .first_mut()
                .unwrap()
                .checks
                .last_mut()
                .unwrap()
                .status = "Unreachable".to_owned();
            check_positive(&unsatisfied, selected).unwrap_err();

            let mut foreign = component_positive(harness);
            foreign.verification_results.results.first_mut().unwrap().harness_id =
                "chat::ledger::parent_identity::proofs::projected_parent_identity_preserves_every_complete_identity_class".to_owned();
            check_positive(&foreign, selected).unwrap_err();
        }
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

    fn negative_field_refusal(pointer: &str, fault: &serde_json::Value) -> String {
        let (mut value, negative) = negative_fixture();
        value.pointer_mut(pointer).unwrap().clone_from(fault);
        let report = domyjob_core::ingress::value(value).unwrap();
        check_negative(&report, &negative).unwrap_err()
    }

    #[test]
    fn each_incomplete_counterexample_field_is_refused_with_a_diagnostic() {
        for (pointer, fault) in summary_faults().into_iter().chain(assertion_faults()) {
            let error = negative_field_refusal(pointer, &fault);
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
                let error = negative_field_refusal(pointer, &fault);
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
