use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use domyjob_core::job_protocol::{Action, State, step};
use serde::Deserialize;

use crate::resource_proofs::{Harness, Negative};

mod handoffs;

const MANIFEST: &str = "verification/obligations.json";
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
const QUINT_VERSION: &str = "0.32.0";
const QUINT_TOOL: &str = "npm:@informalsystems/quint@0.32.0";
const APALACHE_VERSION: &str = "0.56.1";
const MODEL: &str = "verification/job_protocol.qnt";
const TRACE_COUNT: usize = 100;
const TRACE_STEPS: usize = 20;
const TRACE_SEED: &str = "1";
const SCHEMA_VERSION: u32 = 1;
const FUZZ_TOOL: &str = "cargo:cargo-fuzz@0.13.2";
const FUZZ_TARGETS: [(&str, &[&str]); 7] = [
    ("wire", &["hello", "chat-exchange", "chat-unknown-field"]),
    ("source_paths", &["tracked", "metadata", "malformed"]),
    ("process_group", &["positive", "nonpositive", "overflow"]),
    ("chat_ledger", &["notifications"]),
    ("hook_clients", &["foreign", "malformed"]),
    ("terminal_input", &["unicode", "csi", "malformed", "mouse"]),
    (
        "handoff_metadata",
        &[
            "granted",
            "team",
            "malformed",
            "agent-target",
            "team-target",
        ],
    ),
];
const AUTOMATIC_PORT: u16 = 0;
const VERIFIER_HOST: std::net::Ipv4Addr = std::net::Ipv4Addr::LOCALHOST;
const JAVA_TOOL: &str = "java@temurin-25.0.4+101.0.LTS";
const NODE_TOOL: &str = "node@26.11.1";
const MAX_VERSION_BYTES: usize = 256;
const MODEL_FIELD_PARTS: usize = 2;
const MODEL_VARIABLE_COUNT: usize = 4;
const PARENT_POSITIVE: &str = "chat::ledger::parent_identity::proofs::projected_parent_identity_preserves_every_complete_identity_class";
const PARENT_NEGATIVE: &str =
    "chat::ledger::parent_identity::proofs::rejects_a_present_parent_with_a_different_sequence";
const EXIT_SOURCE: &str = "verification/proof-exit-policy.rs";
const EXIT_POSITIVE: &str = "only_the_required_normal_code_admits_each_proof_phase";
const EXIT_NEGATIVE: &str = "rejects_an_unrelated_exit_as_a_named_counterexample";
const EXIT_COVERS: u64 = 4;
const PARENT_COVERS: u64 = 6;
const GENERATOR_COVERS: u64 = 1;
const PRESENT_COVERS: u64 = 4;
const ABSENT_COVERS: u64 = 2;
const GENERATOR_POSITIVE: &str = "chat::ledger::parent_identity::proofs::canonical_parent_identity_generator_preserves_every_origin_byte";
const PRESENT_POSITIVE: &str = "chat::ledger::parent_identity::proofs::present_parent_identity_matches_exact_origin_and_sequence";
const ABSENT_POSITIVE: &str =
    "chat::ledger::parent_identity::proofs::absent_parent_identity_accepts_every_request";
const ENCODER_POSITIVE: &str = "chat::ledger::parent_identity::proofs::canonical_parent_hex_encoder_preserves_the_exact_full_domain";
const ENCODER_NEGATIVE: &str =
    "chat::ledger::parent_identity::proofs::rejects_a_changed_parent_hex_encoder_value";
const ENCODER_COVERS: u64 = 2;
const NATIVE_DEFAULT_SOURCE: &str = "verification/native-default-tool-binding.rs";
const NATIVE_DEFAULT_POSITIVES: [(&str, u64); 5] = [
    (
        "native_default::native_default_loader_search_path_filter",
        2,
    ),
    (
        "native_default::native_default_target_and_object_admission",
        2,
    ),
    (
        "native_default::native_default_required_roles_and_limits",
        4,
    ),
    ("native_default::native_default_full_identity_recheck", 2),
    ("native_default::native_default_builtin_configuration", 2),
];
const NATIVE_DEFAULT_NEGATIVES: [(&str, &str); 5] = [
    (
        "native_default::rejects_unfiltered_native_default_toolchain_loader",
        "NEGATIVE_DEFAULT_LOADER_TAIL",
    ),
    (
        "native_default::rejects_foreign_native_default_architecture",
        "NEGATIVE_DEFAULT_ARCHITECTURE",
    ),
    (
        "native_default::rejects_missing_native_default_required_role",
        "NEGATIVE_DEFAULT_REQUIRED_ROLE",
    ),
    (
        "native_default::rejects_changed_native_default_identity",
        "NEGATIVE_DEFAULT_IDENTITY_CHANGE",
    ),
    (
        "native_default::rejects_foreign_native_default_configuration",
        "NEGATIVE_DEFAULT_CONFIGURATION",
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParentComponent {
    Generator,
    Present,
    Absent,
}

impl ParentComponent {
    fn parse(words: &[&str]) -> Result<Self, String> {
        match words {
            ["generator"] => Ok(Self::Generator),
            ["present"] => Ok(Self::Present),
            ["absent"] => Ok(Self::Absent),
            _ => Err(
                "usage: cargo xtask verify parent-identity-component generator|present|absent"
                    .to_owned(),
            ),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Generator => "generator",
            Self::Present => "present",
            Self::Absent => "absent",
        }
    }

    const fn obligation(self) -> (&'static str, u64) {
        match self {
            Self::Generator => (GENERATOR_POSITIVE, GENERATOR_COVERS),
            Self::Present => (PRESENT_POSITIVE, PRESENT_COVERS),
            Self::Absent => (ABSENT_POSITIVE, ABSENT_COVERS),
        }
    }
}

#[derive(Clone, Copy)]
enum ProofFamily {
    ExitPolicy,
    ParentEncoder,
    ParentIdentity,
    ParentComponent(ParentComponent),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    schema_version: u32,
    kani_version: String,
    quint_version: String,
    apalache_version: String,
    harnesses: Vec<Harness>,
    negatives: Vec<Negative>,
    boundaries: Vec<Boundary>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Boundary {
    source: String,
    reason: String,
    evidence: String,
    reconsider: String,
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = read_bytes(path)?;
    domyjob_core::ingress::json(&bytes, MAX_INPUT_BYTES).map_err(|error| error.to_string())
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let bytes = std::io::Read::bytes(std::io::BufReader::new(file))
        .take(MAX_INPUT_BYTES.saturating_add(1))
        .collect::<std::io::Result<Vec<u8>>>()
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "{} exceeds its verification input bound",
            path.display()
        ));
    }
    Ok(bytes)
}

fn require_source(root: &Path, source: &str) -> Result<(), String> {
    let source = domyjob_core::domain::RelativePath::try_from(source.to_owned())
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(root.join(source.as_str()))
        .map_err(|error| format!("{}: {error}", source.as_str()))?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(format!(
            "verification source is not a regular file: {source}"
        ))
    }
}

fn discover(
    root: &Path,
    directory: &Path,
    files: &mut Vec<std::path::PathBuf>,
) -> Result<(), String> {
    for entry in
        crate::directory_entries(&root.join(directory)).map_err(|error| error.to_string())?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.kind.map_err(|error| error.to_string())?;
        let relative = entry
            .path
            .strip_prefix(root)
            .map_err(|error| error.to_string())?
            .to_path_buf();
        if kind.is_dir() {
            discover(root, &relative, files)?;
        } else if kind.is_file() {
            files.push(relative);
        } else {
            return Err(format!(
                "verification source is not a regular file or directory: {}",
                relative.display()
            ));
        }
    }
    Ok(())
}

fn proof_names(items: &[syn::Item], prefix: &str, names: &mut BTreeSet<String>) {
    for item in items {
        match item {
            syn::Item::Fn(function)
                if function.attrs.iter().any(|attribute| {
                    attribute
                        .path()
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .eq(["kani", "proof"])
                }) =>
            {
                let name = if prefix.is_empty() {
                    function.sig.ident.to_string()
                } else {
                    format!("{prefix}::{}", function.sig.ident)
                };
                names.insert(name);
            }
            syn::Item::Mod(module) => {
                if let Some((_, children)) = &module.content {
                    let prefix = if prefix.is_empty() {
                        module.ident.to_string()
                    } else {
                        format!("{prefix}::{}", module.ident)
                    };
                    proof_names(children, &prefix, names);
                }
            }
            syn::Item::Const(_)
            | syn::Item::Enum(_)
            | syn::Item::ExternCrate(_)
            | syn::Item::Fn(_)
            | syn::Item::ForeignMod(_)
            | syn::Item::Impl(_)
            | syn::Item::Macro(_)
            | syn::Item::Static(_)
            | syn::Item::Struct(_)
            | syn::Item::Trait(_)
            | syn::Item::TraitAlias(_)
            | syn::Item::Type(_)
            | syn::Item::Union(_)
            | syn::Item::Use(_)
            | syn::Item::Verbatim(_)
            | _ => {}
        }
    }
}

fn source_proofs(root: &Path) -> Result<BTreeSet<String>, String> {
    let mut files = Vec::new();
    discover(root, Path::new("crates/domyjob-core/src"), &mut files)?;
    let mut discovered = BTreeSet::new();
    for path in files
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
    {
        let text =
            String::from_utf8(read_bytes(&root.join(&path))?).map_err(|error| error.to_string())?;
        let file = syn::parse_file(&text).map_err(|error| error.to_string())?;
        let relative = path
            .strip_prefix("crates/domyjob-core/src")
            .map_err(|error| error.to_string())?;
        let prefix = relative
            .with_extension("")
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("::");
        proof_names(&file.items, &prefix, &mut discovered);
    }
    Ok(discovered)
}

fn inventory(root: &Path) -> Result<Inventory, String> {
    let inventory: Inventory = read(&root.join(MANIFEST))?;
    if inventory.schema_version != SCHEMA_VERSION
        || inventory.kani_version != crate::resource_proofs::KANI_VERSION
        || inventory.quint_version != QUINT_VERSION
        || inventory.apalache_version != APALACHE_VERSION
        || inventory.harnesses.is_empty()
        || inventory.negatives.is_empty()
        || inventory.boundaries.is_empty()
    {
        return Err(
            "proof inventory has missing obligations or unpinned verifier versions".to_owned(),
        );
    }
    let expected = expected_proofs(root, &inventory)?;
    for boundary in &inventory.boundaries {
        if boundary.reason.trim().is_empty()
            || boundary.evidence.trim().is_empty()
            || boundary.reconsider.trim().is_empty()
        {
            return Err(
                "semantic boundary needs existing source, evidence, reason, and reconsideration"
                    .to_owned(),
            );
        }
        require_source(root, &boundary.source)?;
    }
    let discovered = source_proofs(root)?;
    if discovered != expected.core {
        return Err(format!(
            "proof inventory differs from source: missing {:?}; stale {:?}",
            discovered.difference(&expected.core).collect::<Vec<_>>(),
            expected.core.difference(&discovered).collect::<Vec<_>>()
        ));
    }
    let standalone = standalone_proofs(root)?;
    if standalone != expected.standalone {
        return Err(format!(
            "standalone proof inventory differs from source: expected {:?}; discovered {standalone:?}",
            expected.standalone
        ));
    }
    Ok(inventory)
}

type StandaloneProofs = BTreeMap<String, BTreeSet<String>>;

#[derive(Default)]
struct ExpectedProofs {
    core: BTreeSet<String>,
    standalone: StandaloneProofs,
}

fn register_proof(
    root: &Path,
    source: Option<&str>,
    name: &str,
    expected: &mut ExpectedProofs,
) -> Result<(), String> {
    let names = if let Some(source) = source {
        require_source(root, source)?;
        expected.standalone.entry(source.to_owned()).or_default()
    } else {
        &mut expected.core
    };
    if names.insert(name.to_owned()) {
        Ok(())
    } else {
        Err("proof inventory has a duplicate harness".to_owned())
    }
}

fn expected_proofs(root: &Path, inventory: &Inventory) -> Result<ExpectedProofs, String> {
    let mut expected = ExpectedProofs::default();
    for harness in &inventory.harnesses {
        if harness.name.is_empty()
            || harness.covers == 0
            || harness.contract.trim().is_empty()
            || harness.input_domain.trim().is_empty()
            || harness.bound.trim().is_empty()
            || harness.production.trim().is_empty()
        {
            return Err("proof inventory has an empty, uncovered, or duplicate harness".to_owned());
        }
        register_proof(
            root,
            harness.source.as_deref(),
            &harness.name,
            &mut expected,
        )?;
    }
    for negative in &inventory.negatives {
        if negative.harness.trim().is_empty() || negative.assertion.trim().is_empty() {
            return Err("negative proof needs its named assertion counterexample".to_owned());
        }
        register_proof(
            root,
            negative.source.as_deref(),
            &negative.harness,
            &mut expected,
        )?;
    }
    Ok(expected)
}

fn standalone_proofs(root: &Path) -> Result<StandaloneProofs, String> {
    let mut files = Vec::new();
    discover(root, Path::new("verification"), &mut files)?;
    let mut discovered = BTreeMap::new();
    for path in files
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
    {
        let text =
            String::from_utf8(read_bytes(&root.join(&path))?).map_err(|error| error.to_string())?;
        let file = syn::parse_file(&text).map_err(|error| error.to_string())?;
        let mut names = BTreeSet::new();
        proof_names(&file.items, "", &mut names);
        if !names.is_empty() {
            let path = path
                .components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .ok_or("verification source name is not UTF-8")
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            discovered.insert(path, names);
        }
    }
    Ok(discovered)
}

fn fingerprint(root: &Path) -> Result<blake3::Hash, String> {
    let mut files = Vec::new();
    for directory in [
        "crates",
        "xtask",
        "verification",
        "fuzz/fuzz_targets",
        "fuzz/seeds",
        ".github/workflows",
    ] {
        discover(root, Path::new(directory), &mut files)?;
    }
    for (target, seeds) in FUZZ_TARGETS {
        for seed in seeds {
            files.push(format!("fuzz/corpus/{target}/{seed}").into());
        }
    }
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "mise.toml",
        "rust-toolchain.toml",
        "fuzz/Cargo.toml",
        "fuzz/Cargo.lock",
        "assure.toml",
        ".cargo/config.toml",
        "clippy.toml",
        "rustfmt.toml",
    ] {
        files.push(path.into());
    }
    let mut named = Vec::new();
    for path in files {
        let mut components = Vec::new();
        for component in path.components() {
            components.push(
                component
                    .as_os_str()
                    .to_str()
                    .ok_or("verification source name is not UTF-8")?,
            );
        }
        named.push((components.join("/"), path));
    }
    named.sort_by(|(first, _), (second, _)| first.cmp(second));
    let mut hash = blake3::Hasher::new();
    for (name, path) in named {
        let name = name.as_bytes();
        let contents = read_bytes(&root.join(&path))?;
        hash.update(
            &u64::try_from(name.len())
                .map_err(|error| error.to_string())?
                .to_le_bytes(),
        );
        hash.update(name);
        hash.update(
            &u64::try_from(contents.len())
                .map_err(|error| error.to_string())?
                .to_le_bytes(),
        );
        hash.update(&contents);
    }
    Ok(hash.finalize())
}

#[derive(Debug, Deserialize)]
struct TraceMeta {
    format: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct ModelReport {
    stage: String,
    status: String,
    errors: Vec<serde_json::Value>,
    main: String,
    modules: Vec<serde_json::Value>,
    #[serde(default)]
    trace: Vec<serde_json::Value>,
}

impl ModelReport {
    fn complete(&self, status: &str) -> bool {
        self.stage == "verifying"
            && self.status == status
            && self.errors.is_empty()
            && self.main == "job_protocol"
            && !self.modules.is_empty()
    }
}

fn initial_counterexample(trace: &[serde_json::Value]) -> bool {
    let [record] = trace else {
        return false;
    };
    if record.get("kind").and_then(serde_json::Value::as_str) != Some("app")
        || record.get("opcode").and_then(serde_json::Value::as_str) != Some("Rec")
    {
        return false;
    }
    let Some(arguments) = record.get("args").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let (fields, remainder) = arguments.as_chunks::<MODEL_FIELD_PARTS>();
    if fields.len() != MODEL_VARIABLE_COUNT || !remainder.is_empty() {
        return false;
    }
    let mut seen = BTreeSet::new();
    for [field, value] in fields {
        if field.get("kind").and_then(serde_json::Value::as_str) != Some("str") {
            return false;
        }
        let Some(name) = field.get("value").and_then(serde_json::Value::as_str) else {
            return false;
        };
        if !seen.insert(name) {
            return false;
        }
        if !initial_field(name, value) {
            return false;
        }
    }
    true
}

fn initial_field(name: &str, value: &serde_json::Value) -> bool {
    match name {
        "phase" | "before" => {
            value.get("kind").and_then(serde_json::Value::as_str) == Some("str")
                && value.get("value").and_then(serde_json::Value::as_str) == Some("absent")
        }
        "command" => {
            value.get("kind").and_then(serde_json::Value::as_str) == Some("str")
                && value.get("value").and_then(serde_json::Value::as_str) == Some("init")
        }
        "refused" => {
            value.get("kind").and_then(serde_json::Value::as_str) == Some("bool")
                && value.get("value").and_then(serde_json::Value::as_bool) == Some(false)
        }
        _ => false,
    }
}

fn model_positive(success: bool, report: &ModelReport) -> Result<(), String> {
    if success && report.complete("ok") && report.trace.is_empty() {
        Ok(())
    } else {
        Err("Apalache could not establish the Quint job protocol invariant".to_owned())
    }
}

fn model_negative(success: bool, report: &ModelReport) -> Result<(), String> {
    if !success && report.complete("violation") && initial_counterexample(&report.trace) {
        Ok(())
    } else {
        Err(
            "Apalache must reject the false invariant with its initial-state counterexample"
                .to_owned(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct Trace {
    #[serde(rename = "#meta")]
    metadata: TraceMeta,
    vars: Vec<String>,
    states: Vec<TraceState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Command {
    Init,
    SubmitSame,
    SubmitConflict,
    Attach,
    Disconnect,
    Reap,
}

impl Command {
    const fn action(self) -> Option<Action> {
        match self {
            Self::Init => None,
            Self::SubmitSame => Some(Action::Submit { same_request: true }),
            Self::SubmitConflict => Some(Action::Submit {
                same_request: false,
            }),
            Self::Attach => Some(Action::Attach),
            Self::Disconnect => Some(Action::Disconnect),
            Self::Reap => Some(Action::Reap),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    index: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceState {
    #[serde(rename = "#meta")]
    metadata: Index,
    phase: State,
    before: State,
    command: Command,
    refused: bool,
}

fn replay(trace: &Trace, actions: &mut BTreeSet<Command>) -> Result<(), String> {
    if trace.metadata.format != "ITF"
        || trace.metadata.status != "ok"
        || trace.vars != ["phase", "before", "command", "refused"]
        || trace.states.len() != TRACE_STEPS.saturating_add(1)
    {
        return Err("Quint must emit a complete successful ITF trace".to_owned());
    }
    let Some(initial) = trace.states.first() else {
        return Err("Quint trace has no initial state".to_owned());
    };
    if initial.metadata.index != 0
        || initial.phase != State::Absent
        || initial.before != State::Absent
        || initial.command != Command::Init
        || initial.refused
    {
        return Err("Quint trace has an invalid initial state".to_owned());
    }
    let mut state = State::Absent;
    for (index, observed) in trace.states.iter().enumerate().skip(1) {
        let action = observed
            .command
            .action()
            .ok_or_else(|| "Quint reinitialized during a trace".to_owned())?;
        if observed.metadata.index != index || observed.before != state {
            return Err("Quint trace has a missing or disconnected transition".to_owned());
        }
        let result = step(state, action);
        let (next, refused) = match result {
            Ok(next) => (next, false),
            Err(_refusal) => (state, true),
        };
        if refused != observed.refused || next != observed.phase {
            return Err(format!(
                "Rust job protocol disagrees with Quint at step {index}: {:?}",
                observed.command
            ));
        }
        state = observed.phase;
        actions.insert(observed.command);
    }
    Ok(())
}

fn version_bytes(stdout: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut stdout = std::io::BufReader::new(stdout);
    let retained = std::io::Read::bytes(std::io::Read::by_ref(&mut stdout))
        .take(MAX_VERSION_BYTES.saturating_add(1))
        .collect::<std::io::Result<Vec<u8>>>();
    let drained = std::io::copy(&mut stdout, &mut std::io::sink());
    retained.and_then(|bytes| drained.map(|_discarded| bytes))
}

fn check_quint_version(root: &Path) -> Result<(), String> {
    let mut version = crate::raw::command("mise")
        .current_dir(root)
        .args(["x", QUINT_TOOL, "--", "quint", "--version"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = version
        .stdout
        .take()
        .ok_or_else(|| "Quint version command has no output pipe".to_owned())?;
    let bytes = version_bytes(stdout);
    let status = version.wait().map_err(|error| error.to_string())?;
    let bytes = bytes.map_err(|error| error.to_string())?;
    if !status.success()
        || bytes.len() > MAX_VERSION_BYTES
        || String::from_utf8_lossy(&bytes).trim() != QUINT_VERSION
    {
        return Err("Quint version does not match the proof inventory".to_owned());
    }
    Ok(())
}

fn verify_model(root: &Path, directory: &Path) -> Result<(), String> {
    let listener = std::net::TcpListener::bind((VERIFIER_HOST, AUTOMATIC_PORT))
        .map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    drop(listener);
    let report = directory.join("model.json");
    let verification = crate::raw::command("mise")
        .current_dir(directory)
        .args([
            "x", QUINT_TOOL, JAVA_TOOL, NODE_TOOL, "--", "quint", "verify",
        ])
        .arg(root.join(MODEL))
        .args([
            "--backend=apalache",
            "--invariant=durable",
            "--inductive-invariant=inductive",
            "--verbosity=1",
        ])
        .arg(format!("--server-endpoint={VERIFIER_HOST}:{port}"))
        .arg(format!("--apalache-version={APALACHE_VERSION}"))
        .arg(format!("--max-steps={TRACE_STEPS}"))
        .arg(format!("--out={}", report.display()))
        .status()
        .map_err(|error| error.to_string())?;
    let positive: ModelReport = read(&report)?;
    model_positive(verification.success(), &positive)?;
    let negative_report = directory.join("model-negative.json");
    let negative = crate::raw::command("mise")
        .current_dir(directory)
        .args([
            "x", QUINT_TOOL, JAVA_TOOL, NODE_TOOL, "--", "quint", "verify",
        ])
        .arg(root.join(MODEL))
        .args([
            "--backend=apalache",
            "--invariant=false",
            "--max-steps=0",
            "--verbosity=1",
        ])
        .arg(format!("--server-endpoint={VERIFIER_HOST}:{port}"))
        .arg(format!("--apalache-version={APALACHE_VERSION}"))
        .arg(format!("--out={}", negative_report.display()))
        .status()
        .map_err(|error| error.to_string())?;
    let counterexample: ModelReport = read(&negative_report)?;
    model_negative(negative.success(), &counterexample)?;
    Ok(())
}

fn generate_traces(root: &Path, directory: &Path) -> Result<(), String> {
    let status = crate::raw::command("mise")
        .current_dir(root)
        .args([
            "x",
            QUINT_TOOL,
            "--",
            "quint",
            "run",
            MODEL,
            "--backend=typescript",
            "--invariant=durable",
            "--verbosity=0",
        ])
        .arg(format!("--seed={TRACE_SEED}"))
        .arg(format!("--max-samples={TRACE_COUNT}"))
        .arg(format!("--n-traces={TRACE_COUNT}"))
        .arg(format!("--max-steps={TRACE_STEPS}"))
        .arg(format!(
            "--out-itf={}/trace-{{seq}}.itf.json",
            directory.display()
        ))
        .status()
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err("Quint job protocol invariant failed".to_owned());
    }
    Ok(())
}

fn replay_traces(directory: &Path) -> Result<(), String> {
    let mut actions = BTreeSet::new();
    for index in 0..TRACE_COUNT {
        replay(
            &read(&directory.join(format!("trace-{index}.itf.json")))?,
            &mut actions,
        )?;
    }
    if actions
        != BTreeSet::from([
            Command::SubmitSame,
            Command::SubmitConflict,
            Command::Attach,
            Command::Disconnect,
            Command::Reap,
        ])
    {
        return Err(format!(
            "Quint replay did not cover every job protocol action: {actions:?}"
        ));
    }
    eprintln!(
        "verification: replayed {TRACE_COUNT} Quint traces through production job_protocol::step"
    );
    Ok(())
}

fn quint(root: &Path) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    check_quint_version(root)?;
    verify_model(root, directory.path())?;
    generate_traces(root, directory.path())?;
    replay_traces(directory.path())
}

pub fn run(root: &Path) -> Result<(), String> {
    let before = fingerprint(root)?;
    let inventory = bound_attempt(
        || {
            check_parent_refinement(root)?;
            let inventory = inventory(root)?;
            quint(root)?;
            crate::resource_proofs::run_inventory(
                root,
                &inventory.harnesses,
                &inventory.negatives,
            )?;
            replay_fuzz(root)?;
            Ok(inventory)
        },
        || unchanged_source(root, before),
    )?;
    eprintln!(
        "verification: source={before}; {} positive harnesses; {} negative controls; {TRACE_COUNT} ITF traces; {} preserved corpora",
        inventory.harnesses.len(),
        inventory.negatives.len(),
        FUZZ_TARGETS.len()
    );
    Ok(())
}

fn bound_attempt<T>(
    attempt: impl FnOnce() -> Result<T, String>,
    after: impl FnOnce() -> Result<(), String>,
) -> Result<T, String> {
    let result = attempt();
    let binding = after();
    match (result, binding) {
        (Ok(inventory), Ok(())) => Ok(inventory),
        (Err(primary), Ok(())) | (Ok(_), Err(primary)) => Err(primary),
        (Err(primary), Err(binding_failure)) => {
            Err(format!("{primary}; source binding: {binding_failure}"))
        }
    }
}

fn select_proof_pair(
    inventory: &Inventory,
    family: ProofFamily,
) -> Result<(&Harness, &Negative), String> {
    let (positive_name, negative_name, source, covers, assertion) = match family {
        ProofFamily::ExitPolicy => (
            EXIT_POSITIVE,
            EXIT_NEGATIVE,
            Some(EXIT_SOURCE),
            EXIT_COVERS,
            "NEGATIVE_PROOF_EXIT_CODE",
        ),
        ProofFamily::ParentEncoder => (
            ENCODER_POSITIVE,
            ENCODER_NEGATIVE,
            None,
            ENCODER_COVERS,
            "NEGATIVE_PARENT_HEX_ENCODER",
        ),
        ProofFamily::ParentIdentity => (
            PARENT_POSITIVE,
            PARENT_NEGATIVE,
            None,
            PARENT_COVERS,
            "NEGATIVE_PARENT_IDENTITY",
        ),
        ProofFamily::ParentComponent(component) => {
            let (name, covers) = component.obligation();
            (
                name,
                PARENT_NEGATIVE,
                None,
                covers,
                "NEGATIVE_PARENT_IDENTITY",
            )
        }
    };
    let positives = inventory
        .harnesses
        .iter()
        .filter(|harness| harness.name == positive_name)
        .collect::<Vec<_>>();
    let negatives = inventory
        .negatives
        .iter()
        .filter(|negative| negative.harness == negative_name)
        .collect::<Vec<_>>();
    let ([positive], [negative]) = (positives.as_slice(), negatives.as_slice()) else {
        return Err("selected proof pair is missing or duplicated".to_owned());
    };
    if positive.source.as_deref() != source
        || negative.source.as_deref() != source
        || positive.covers != covers
        || negative.assertion != assertion
    {
        return Err(
            "selected proof pair has a foreign source, cover count, or assertion".to_owned(),
        );
    }
    Ok((positive, negative))
}

fn parent_identity(root: &Path) -> Result<(), String> {
    crate::resource_proofs::process::require_native_linux()?;
    let before = fingerprint(root)?;
    bound_attempt(
        || {
            check_parent_refinement(root)?;
            let inventory = inventory(root)?;
            let (exit_positive, exit_negative) =
                select_proof_pair(&inventory, ProofFamily::ExitPolicy)?;
            let (parent_positive, parent_negative) =
                select_proof_pair(&inventory, ProofFamily::ParentIdentity)?;
            crate::resource_proofs::run_inventory(
                root,
                std::slice::from_ref(exit_positive),
                std::slice::from_ref(exit_negative),
            )?;
            crate::resource_proofs::run_inventory(
                root,
                std::slice::from_ref(parent_positive),
                std::slice::from_ref(parent_negative),
            )
        },
        || unchanged_source(root, before),
    )?;
    eprintln!(
        "verification: source={before}; native-default tool binding; exit policy 1 positive/1 negative/4 covers; Parent 1 positive/1 negative/6 covers"
    );
    Ok(())
}

fn parent_identity_encoder(root: &Path) -> Result<(), String> {
    crate::resource_proofs::process::require_native_linux()?;
    let before = fingerprint(root)?;
    bound_attempt(
        || {
            check_parent_refinement(root)?;
            let inventory = inventory(root)?;
            let (exit_positive, exit_negative) =
                select_proof_pair(&inventory, ProofFamily::ExitPolicy)?;
            let (positive, negative) = select_proof_pair(&inventory, ProofFamily::ParentEncoder)?;
            crate::resource_proofs::run_inventory(
                root,
                std::slice::from_ref(exit_positive),
                std::slice::from_ref(exit_negative),
            )?;
            crate::resource_proofs::run_inventory(
                root,
                std::slice::from_ref(positive),
                std::slice::from_ref(negative),
            )
        },
        || unchanged_source(root, before),
    )?;
    eprintln!(
        "verification: source={before}; native-default tool binding; exit policy 1 positive/1 negative/{EXIT_COVERS} covers; Parent encoder 1 positive/1 negative/{ENCODER_COVERS} covers; generator and aggregate proofs remain required"
    );
    Ok(())
}

fn parent_identity_component(root: &Path, component: ParentComponent) -> Result<(), String> {
    crate::resource_proofs::process::require_native_linux()?;
    let before = fingerprint(root)?;
    bound_attempt(
        || {
            check_parent_refinement(root)?;
            let inventory = inventory(root)?;
            let (exit_positive, exit_negative) =
                select_proof_pair(&inventory, ProofFamily::ExitPolicy)?;
            let (component_positive, parent_negative) =
                select_proof_pair(&inventory, ProofFamily::ParentComponent(component))?;
            crate::resource_proofs::run_inventory(
                root,
                std::slice::from_ref(exit_positive),
                std::slice::from_ref(exit_negative),
            )?;
            crate::resource_proofs::run_inventory(
                root,
                std::slice::from_ref(component_positive),
                std::slice::from_ref(parent_negative),
            )
        },
        || unchanged_source(root, before),
    )?;
    let (_, covers) = component.obligation();
    eprintln!(
        "verification: source={before}; native-default tool binding; selected Parent {} component; exit policy 1 positive/1 negative/{EXIT_COVERS} covers; component 1 positive/1 negative/{covers} covers; aggregate proof remains required",
        component.name()
    );
    Ok(())
}

fn select_native_default(inventory: &Inventory) -> Result<(Vec<Harness>, Vec<Negative>), String> {
    let harnesses = inventory
        .harnesses
        .iter()
        .filter(|item| item.source.as_deref() == Some(NATIVE_DEFAULT_SOURCE))
        .cloned()
        .collect::<Vec<_>>();
    let negatives = inventory
        .negatives
        .iter()
        .filter(|item| item.source.as_deref() == Some(NATIVE_DEFAULT_SOURCE))
        .cloned()
        .collect::<Vec<_>>();
    let names = harnesses
        .iter()
        .map(|item| (item.name.as_str(), item.covers))
        .collect::<BTreeMap<_, _>>();
    let assertions = negatives
        .iter()
        .map(|item| (item.harness.as_str(), item.assertion.as_str()))
        .collect::<BTreeMap<_, _>>();
    if harnesses.len() != NATIVE_DEFAULT_POSITIVES.len()
        || negatives.len() != NATIVE_DEFAULT_NEGATIVES.len()
        || names != BTreeMap::from(NATIVE_DEFAULT_POSITIVES)
        || assertions != BTreeMap::from(NATIVE_DEFAULT_NEGATIVES)
    {
        return Err("native-default qualification inventory changed its exact contract".to_owned());
    }
    Ok((harnesses, negatives))
}

fn native_default(root: &Path) -> Result<(), String> {
    crate::resource_proofs::process::require_native_linux()?;
    let before = fingerprint(root)?;
    bound_attempt(
        || {
            check_parent_refinement(root)?;
            let inventory = inventory(root)?;
            let (harnesses, negatives) = select_native_default(&inventory)?;
            crate::resource_proofs::run_inventory(root, &harnesses, &negatives)
        },
        || unchanged_source(root, before),
    )?;
    eprintln!(
        "verification: source={before}; native-default qualification; 5 positives; 5 named negatives; 12 covers; source and physical tools bound before and after"
    );
    Ok(())
}

fn unchanged_source(root: &Path, before: blake3::Hash) -> Result<(), String> {
    if fingerprint(root)? == before {
        Ok(())
    } else {
        Err("source changed while verification was running; rerun the current source".to_owned())
    }
}

pub fn replay_models(root: &Path) -> Result<(), String> {
    quint(root)
}

pub fn dispatch(root: &Path, words: &[&str]) -> Result<(), String> {
    match words {
        ["__native-default-context"] => crate::proof_tools::emit_default_selection(),
        ["__native-default-versions"] => crate::proof_tools::emit_default_versions(root),
        [] => run(root),
        ["models"] => replay_models(root),
        ["inventory"] => check_inventory(root),
        ["handoffs"] => handoffs::run(root),
        ["native-default"] => native_default(root),
        ["parent-identity-encoder"] => parent_identity_encoder(root),
        ["parent-identity"] => parent_identity(root),
        ["parent-identity-component", component @ ..] => {
            parent_identity_component(root, ParentComponent::parse(component)?)
        }
        ["fuzz"] => replay_fuzz(root),
        _ => Err("usage: cargo xtask verify [models|inventory|fuzz|handoffs|native-default|parent-identity|parent-identity-encoder|parent-identity-component generator|present|absent]".to_owned()),
    }
}

pub fn check_inventory(root: &Path) -> Result<(), String> {
    check_parent_refinement(root)?;
    inventory(root).map(|_inventory| ())
}

pub fn check_parent_refinement(root: &Path) -> Result<(), String> {
    let sources = crate::parent_refinement::FILES
        .iter()
        .map(|file| {
            String::from_utf8(read_bytes(&root.join(file))?).map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, String>>()?;
    crate::parent_refinement::check(&sources).map_err(|error| error.to_string())
}

pub fn replay_fuzz(root: &Path) -> Result<(), String> {
    for (target, seeds) in FUZZ_TARGETS {
        replay_target(root, target, seeds)?;
    }
    Ok(())
}

fn replay_target(root: &Path, target: &str, seeds: &[&str]) -> Result<(), String> {
    if seeds.is_empty() {
        return Err("source ingress requires a nonempty replay corpus".to_owned());
    }
    let corpus = tempfile::tempdir().map_err(|error| error.to_string())?;
    for seed in seeds {
        let relative = format!("fuzz/corpus/{target}/{seed}");
        require_source(root, &relative)?;
        let bytes = read_bytes(&root.join(relative))?;
        if bytes.is_empty() {
            return Err("source ingress replay seeds cannot be empty".to_owned());
        }
        crate::raw::write(&corpus.path().join(seed), &bytes).map_err(|error| error.to_string())?;
    }
    let native = match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        _ => {
            return Err(
                "the preserved libFuzzer corpus requires a supported native Unix target".to_owned(),
            );
        }
    };
    let status = crate::raw::command("mise")
        .current_dir(root.join("fuzz"))
        .args([
            "x",
            FUZZ_TOOL,
            "--",
            "cargo",
            "+nightly-2026-10-01",
            "fuzz",
            "run",
            target,
        ])
        .arg(corpus.path())
        .args([
            "--target",
            native,
            "--",
            "-runs=0",
            "-seed=1",
            "-verbosity=0",
        ])
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ingress corpus replay failed: {target}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Trace, replay};
    use std::collections::BTreeSet;

    fn actual_inventory_value() -> serde_json::Value {
        domyjob_core::ingress::foreign_json(include_str!("../../verification/obligations.json"))
            .unwrap()
    }

    fn native_default_inventory_fixture() -> super::Inventory {
        let mut inventory: super::Inventory =
            domyjob_core::ingress::value(actual_inventory_value()).unwrap();
        inventory
            .harnesses
            .retain(|item| item.source.as_deref() == Some(super::NATIVE_DEFAULT_SOURCE));
        inventory
            .negatives
            .retain(|item| item.source.as_deref() == Some(super::NATIVE_DEFAULT_SOURCE));
        inventory
    }

    #[test]
    fn native_default_selector_requires_its_exact_registered_contracts() {
        #[derive(Clone, Copy, Debug)]
        enum Change {
            PositiveSource,
            PositiveName,
            PositiveCovers,
            DuplicatePositive,
            NegativeSource,
            NegativeAssertion,
            DuplicateNegative,
            MissingNegatives,
        }

        let (harnesses, negatives) =
            super::select_native_default(&native_default_inventory_fixture()).unwrap();
        assert_eq!(harnesses.len(), super::NATIVE_DEFAULT_POSITIVES.len());
        assert_eq!(negatives.len(), super::NATIVE_DEFAULT_NEGATIVES.len());
        for change in [
            Change::PositiveSource,
            Change::PositiveName,
            Change::PositiveCovers,
            Change::DuplicatePositive,
            Change::NegativeSource,
            Change::NegativeAssertion,
            Change::DuplicateNegative,
            Change::MissingNegatives,
        ] {
            let mut inventory = native_default_inventory_fixture();
            match change {
                Change::PositiveSource => {
                    inventory.harnesses.first_mut().unwrap().source = None;
                }
                Change::PositiveName => {
                    inventory
                        .harnesses
                        .first_mut()
                        .unwrap()
                        .name
                        .push_str("_foreign");
                }
                Change::PositiveCovers => {
                    inventory.harnesses.first_mut().unwrap().covers = 0;
                }
                Change::DuplicatePositive => {
                    inventory
                        .harnesses
                        .push(inventory.harnesses.first().unwrap().clone());
                }
                Change::NegativeSource => {
                    inventory.negatives.first_mut().unwrap().source = None;
                }
                Change::NegativeAssertion => {
                    inventory
                        .negatives
                        .first_mut()
                        .unwrap()
                        .assertion
                        .push_str("_foreign");
                }
                Change::DuplicateNegative => {
                    inventory
                        .negatives
                        .push(inventory.negatives.first().unwrap().clone());
                }
                Change::MissingNegatives => inventory.negatives.clear(),
            }
            assert!(
                super::select_native_default(&inventory).is_err(),
                "{change:?}"
            );
        }
    }

    fn actual_pair_indices(family: super::ProofFamily) -> (usize, usize) {
        let inventory: super::Inventory =
            domyjob_core::ingress::value(actual_inventory_value()).unwrap();
        let (positive, negative) = super::select_proof_pair(&inventory, family).unwrap();
        let positive_index = inventory
            .harnesses
            .iter()
            .position(|candidate| std::ptr::eq(candidate, positive))
            .unwrap();
        let negative_index = inventory
            .negatives
            .iter()
            .position(|candidate| std::ptr::eq(candidate, negative))
            .unwrap();
        (positive_index, negative_index)
    }

    fn actual_families() -> [super::ProofFamily; 6] {
        [
            super::ProofFamily::ExitPolicy,
            super::ProofFamily::ParentEncoder,
            super::ProofFamily::ParentIdentity,
            super::ProofFamily::ParentComponent(super::ParentComponent::Generator),
            super::ProofFamily::ParentComponent(super::ParentComponent::Present),
            super::ProofFamily::ParentComponent(super::ParentComponent::Absent),
        ]
    }

    #[test]
    fn encoder_selector_has_its_own_exact_pair_and_refuses_extra_arguments() {
        let inventory: super::Inventory =
            domyjob_core::ingress::value(actual_inventory_value()).unwrap();
        let (positive, negative) =
            super::select_proof_pair(&inventory, super::ProofFamily::ParentEncoder).unwrap();
        assert_eq!(positive.name, super::ENCODER_POSITIVE);
        assert_eq!(positive.covers, super::ENCODER_COVERS);
        assert_eq!(negative.harness, super::ENCODER_NEGATIVE);
        assert_eq!(negative.assertion, "NEGATIVE_PARENT_HEX_ENCODER");
        assert_ne!(negative.harness, super::PARENT_NEGATIVE);
        let root = tempfile::tempdir().unwrap();
        for words in [
            vec!["parent-identity-encoder", "extra"],
            vec![
                "parent-identity-encoder",
                "--harness",
                super::ENCODER_POSITIVE,
            ],
            vec!["parent-identity-encoder", "--backend", "z3"],
        ] {
            assert!(
                super::dispatch(root.path(), &words)
                    .unwrap_err()
                    .starts_with("usage:")
            );
        }
    }

    #[test]
    fn component_arguments_select_only_the_three_typed_cases() {
        for (word, expected) in [
            ("generator", super::ParentComponent::Generator),
            ("present", super::ParentComponent::Present),
            ("absent", super::ParentComponent::Absent),
        ] {
            assert_eq!(super::ParentComponent::parse(&[word]).unwrap(), expected);
            assert_eq!(expected.name(), word);
        }
        for words in [
            vec![],
            vec![""],
            vec!["Generator"],
            vec!["aggregate"],
            vec![super::PARENT_POSITIVE],
            vec!["verification/proof-exit-policy.rs"],
            vec!["generator", "present"],
            vec!["present", "--solver=z3"],
            vec!["absent", "--harness-timeout=120s"],
        ] {
            assert!(
                super::ParentComponent::parse(&words)
                    .unwrap_err()
                    .starts_with("usage:")
            );
        }
    }

    #[test]
    fn selected_components_keep_the_original_parent_negative_and_cover_contracts() {
        let inventory: super::Inventory =
            domyjob_core::ingress::value(actual_inventory_value()).unwrap();
        for (component, name, covers) in [
            (
                super::ParentComponent::Generator,
                super::GENERATOR_POSITIVE,
                super::GENERATOR_COVERS,
            ),
            (
                super::ParentComponent::Present,
                super::PRESENT_POSITIVE,
                super::PRESENT_COVERS,
            ),
            (
                super::ParentComponent::Absent,
                super::ABSENT_POSITIVE,
                super::ABSENT_COVERS,
            ),
        ] {
            let (positive, negative) = super::select_proof_pair(
                &inventory,
                super::ProofFamily::ParentComponent(component),
            )
            .unwrap();
            assert_eq!(positive.name, name);
            assert_eq!(positive.covers, covers);
            assert_ne!(positive.name, super::PARENT_POSITIVE);
            assert_eq!(negative.harness, super::PARENT_NEGATIVE);
            assert_eq!(negative.assertion, "NEGATIVE_PARENT_IDENTITY");
        }
    }

    #[test]
    fn component_refinement_rejects_parsed_domain_assertion_and_cover_changes() {
        let original = [
            include_str!("../../crates/domyjob-core/src/chat/ledger.rs"),
            include_str!("../../crates/domyjob-core/src/chat/ledger/parent_identity.rs"),
            include_str!("../../crates/domyjob-core/src/chat/id.rs"),
            include_str!("../../crates/domyjob-core/src/chat/event.rs"),
            include_str!("../../crates/domyjob/src/chat/store/views.rs"),
            include_str!("../../crates/domyjob-core/src/lib.rs"),
            include_str!("../../crates/domyjob-core/src/chat.rs"),
        ]
        .map(str::to_owned);
        crate::parent_refinement::check(&original).unwrap();
        let identity_index = crate::parent_refinement::FILES
            .iter()
            .position(|file| *file == "crates/domyjob-core/src/chat/ledger/parent_identity.rs")
            .unwrap();
        let identity = original.get(identity_index).unwrap();
        for (before, after) in [
            (
                "const ORIGIN_BYTES: usize = 32;",
                "const ORIGIN_BYTES: usize = 16;",
            ),
            (
                "NonZeroU64::new(kani::any())",
                "NonZeroU64::new(u64::from(kani::any::<u32>()))",
            ),
            (
                "let generated = identity().unwrap();",
                "let generated = identity().unwrap(); kani::assume(false);",
            ),
            (
                "fn present_parent_identity_matches_exact_origin_and_sequence() {\n        let parent = identity().unwrap();\n        let request = identity().unwrap();\n        assert_eq!(parent.identity.origin().as_str().as_bytes(), parent.bytes);",
                "fn present_parent_identity_matches_exact_origin_and_sequence() {\n        let parent = identity().unwrap();\n        let request = identity().unwrap();",
            ),
            (
                "fn absent_parent_identity_accepts_every_request() {\n        let parent = identity().unwrap();\n        let request = identity().unwrap();\n        assert_eq!(parent.identity.origin().as_str().as_bytes(), parent.bytes);",
                "fn absent_parent_identity_accepts_every_request() {\n        let request = identity().unwrap();",
            ),
            (
                "matches(Some(&parent.identity), &request.identity)",
                "matches(None, &request.identity)",
            ),
            (
                "kani::cover!(origin_equal && !sequence_equal && !admitted);",
                "",
            ),
            ("assert!(missing_admitted);", "let _ = missing_admitted;"),
            ("NEGATIVE_PARENT_IDENTITY", "UNRELATED_PARENT_ASSERTION"),
        ] {
            let changed = identity.replacen(before, after, 1);
            assert_ne!(&changed, identity);
            syn::parse_file(&changed).unwrap();
            let mut sources = original.clone();
            *sources.get_mut(identity_index).unwrap() = changed;
            assert!(matches!(
                crate::parent_refinement::check(&sources),
                Err(crate::parent_refinement::RefinementError::Refused { .. })
            ));
        }
    }

    #[test]
    fn closed_calibration_selects_registered_exit_and_parent_objects() {
        let inventory: super::Inventory =
            domyjob_core::ingress::value(actual_inventory_value()).unwrap();
        let (exit, exit_negative) =
            super::select_proof_pair(&inventory, super::ProofFamily::ExitPolicy).unwrap();
        assert_eq!(exit.name, super::EXIT_POSITIVE);
        assert_eq!(exit_negative.harness, super::EXIT_NEGATIVE);
        let (parent, parent_negative) =
            super::select_proof_pair(&inventory, super::ProofFamily::ParentIdentity).unwrap();
        assert_eq!(parent.name, super::PARENT_POSITIVE);
        assert_eq!(parent_negative.harness, super::PARENT_NEGATIVE);
        for family in actual_families() {
            let (positive_index, negative_index) = actual_pair_indices(family);
            let (positive, negative) = super::select_proof_pair(&inventory, family).unwrap();
            assert!(std::ptr::eq(
                positive,
                inventory.harnesses.get(positive_index).unwrap()
            ));
            assert!(std::ptr::eq(
                negative,
                inventory.negatives.get(negative_index).unwrap()
            ));
        }
    }

    #[test]
    fn closed_calibration_refuses_missing_duplicate_and_foreign_pairs() {
        for family in actual_families() {
            let (positive, negative) = actual_pair_indices(family);
            let original = actual_inventory_value();
            let mut mutations = Vec::new();
            for (array, index) in [("harnesses", positive), ("negatives", negative)] {
                let mut missing = original.clone();
                missing
                    .get_mut(array)
                    .unwrap()
                    .as_array_mut()
                    .unwrap()
                    .remove(index);
                mutations.push(missing);
                let mut duplicate = original.clone();
                let entries = duplicate.get_mut(array).unwrap().as_array_mut().unwrap();
                entries.push(entries.get(index).unwrap().clone());
                mutations.push(duplicate);
            }
            for (path, value) in [
                (
                    format!("/harnesses/{positive}/name"),
                    serde_json::json!("UNREGISTERED_COMPONENT"),
                ),
                (
                    format!("/negatives/{negative}/harness"),
                    serde_json::json!("UNREGISTERED_NEGATIVE"),
                ),
                (
                    format!("/harnesses/{positive}/source"),
                    serde_json::json!("verification/resource-negative.rs"),
                ),
                (
                    format!("/negatives/{negative}/source"),
                    serde_json::json!("verification/resource-negative.rs"),
                ),
                (
                    format!("/harnesses/{positive}/covers"),
                    serde_json::json!(0),
                ),
                (
                    format!("/harnesses/{positive}/covers"),
                    serde_json::json!(u64::MAX),
                ),
                (
                    format!("/negatives/{negative}/assertion"),
                    serde_json::json!("UNRELATED_ASSERTION"),
                ),
            ] {
                let mut changed = original.clone();
                *changed.pointer_mut(&path).unwrap() = value;
                mutations.push(changed);
            }
            for changed in mutations {
                let inventory = domyjob_core::ingress::value(changed).unwrap();
                super::select_proof_pair(&inventory, family).unwrap_err();
            }
        }
    }

    #[test]
    fn empty_work_cannot_select_any_calibration_pair() {
        let mut value = actual_inventory_value();
        *value.get_mut("harnesses").unwrap() = serde_json::json!([]);
        *value.get_mut("negatives").unwrap() = serde_json::json!([]);
        let inventory = domyjob_core::ingress::value(value).unwrap();
        for family in actual_families() {
            super::select_proof_pair(&inventory, family).unwrap_err();
        }
    }

    #[test]
    fn calibration_does_not_accept_extra_arguments_or_backend_selection() {
        let root = std::path::Path::new("unused");
        for words in [
            vec!["parent-identity", "extra"],
            vec!["parent-identity", "--solver=z3"],
            vec!["parent-identity", "--harness-timeout=120s"],
            vec!["parent-identity-component"],
            vec!["parent-identity-component", ""],
            vec!["parent-identity-component", "aggregate"],
            vec!["parent-identity-component", super::PARENT_POSITIVE],
            vec!["parent-identity-component", "generator", "present"],
            vec!["parent-identity-component", "present", "--solver=z3"],
            vec![
                "parent-identity-component",
                "absent",
                "--harness-timeout=120s",
            ],
        ] {
            assert!(
                super::dispatch(root, &words)
                    .unwrap_err()
                    .starts_with("usage:")
            );
        }
    }

    #[test]
    fn actual_bound_attempt_runs_after_every_primary_outcome_and_retains_both_errors() {
        for primary_ok in [false, true] {
            for after_ok in [false, true] {
                let calls = std::cell::RefCell::new(Vec::new());
                let outcome = super::bound_attempt(
                    || {
                        calls.borrow_mut().push("primary");
                        if primary_ok {
                            Ok("completed")
                        } else {
                            Err("primary refusal".to_owned())
                        }
                    },
                    || {
                        calls.borrow_mut().push("after");
                        if after_ok {
                            Ok(())
                        } else {
                            Err("changed source".to_owned())
                        }
                    },
                );
                assert_eq!(*calls.borrow(), ["primary", "after"]);
                let expected = match (primary_ok, after_ok) {
                    (true, true) => Ok("completed"),
                    (false, true) => Err("primary refusal".to_owned()),
                    (true, false) => Err("changed source".to_owned()),
                    (false, false) => {
                        Err("primary refusal; source binding: changed source".to_owned())
                    }
                };
                assert_eq!(outcome, expected);
            }
        }
    }

    #[test]
    fn oversized_version_output_is_drained_before_waiting_for_completion() {
        for length in [
            super::MAX_VERSION_BYTES,
            super::MAX_VERSION_BYTES + 1,
            super::MAX_VERSION_BYTES * 1024,
        ] {
            let mut output = std::io::Cursor::new(vec![b'x'; length]);
            let retained = super::version_bytes(&mut output).unwrap();
            assert_eq!(retained.len(), length.min(super::MAX_VERSION_BYTES + 1));
            assert_eq!(output.position(), u64::try_from(length).unwrap());
        }
    }

    fn model_report() -> super::ModelReport {
        super::ModelReport {
            stage: "verifying".to_owned(),
            status: "ok".to_owned(),
            errors: vec![],
            main: "job_protocol".to_owned(),
            modules: vec![serde_json::json!({})],
            trace: vec![],
        }
    }

    #[test]
    fn solver_failure_or_malformed_counterexample_cannot_pass_a_model_control() {
        let mut report = model_report();
        super::model_positive(true, &report).unwrap();
        super::model_positive(false, &report).unwrap_err();
        report.status = "violation".to_owned();
        report.trace = vec![serde_json::json!({"kind":"app","opcode":"Rec","args":[
            {"kind":"str","value":"phase"},{"kind":"str","value":"absent"},
            {"kind":"str","value":"before"},{"kind":"str","value":"absent"},
            {"kind":"str","value":"command"},{"kind":"str","value":"init"},
            {"kind":"str","value":"refused"},{"kind":"bool","value":false}
        ]})];
        super::model_negative(false, &report).unwrap();
        super::model_negative(true, &report).unwrap_err();
        super::model_positive(true, &report).unwrap_err();
        report.status = "failure".to_owned();
        super::model_negative(false, &report).unwrap_err();
        report.status = "violation".to_owned();
        report.errors.push(serde_json::json!("solver failed"));
        super::model_negative(false, &report).unwrap_err();
        report.errors.clear();
        report.trace = vec![serde_json::json!({})];
        super::model_negative(false, &report).unwrap_err();
    }

    #[test]
    fn model_refusals_preserve_the_reason_for_the_failed_proof_or_control() {
        let report = model_report();
        let proof = super::model_positive(false, &report).unwrap_err();
        assert_ne!(proof.trim(), "");
        let control = super::model_negative(false, &report).unwrap_err();
        assert_ne!(control.trim(), "");
    }

    fn fingerprint_fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for directory in [
            "crates",
            "xtask/src",
            "verification",
            "fuzz/fuzz_targets",
            "fuzz/seeds",
            ".github/workflows",
            ".cargo",
        ] {
            crate::raw::create_dir_all(&root.path().join(directory)).unwrap();
        }
        for file in [
            "Cargo.toml",
            "Cargo.lock",
            "mise.toml",
            "rust-toolchain.toml",
            "fuzz/Cargo.toml",
            "fuzz/Cargo.lock",
            "assure.toml",
            ".cargo/config.toml",
            "clippy.toml",
            "rustfmt.toml",
        ] {
            crate::raw::write(&root.path().join(file), b"fixture").unwrap();
        }
        for (target, seeds) in super::FUZZ_TARGETS {
            let directory = root.path().join("fuzz/corpus").join(target);
            crate::raw::create_dir_all(&directory).unwrap();
            for seed in seeds {
                crate::raw::write(&directory.join(seed), b"fixture").unwrap();
            }
        }
        root
    }

    #[test]
    fn verifier_manifest_changes_refuse_a_stale_source_receipt() {
        let root = fingerprint_fixture();
        let manifest = root.path().join("xtask/Cargo.toml");
        crate::raw::write(&manifest, b"[features]\nchecked = []\n").unwrap();
        let before = super::fingerprint(root.path()).unwrap();
        super::unchanged_source(root.path(), before).unwrap();
        crate::raw::write(&manifest, b"[features]\nchanged = []\n").unwrap();
        super::unchanged_source(root.path(), before).unwrap_err();
    }

    #[test]
    fn adding_an_automatic_verifier_build_script_refuses_a_stale_source_receipt() {
        let root = fingerprint_fixture();
        let before = super::fingerprint(root.path()).unwrap();
        super::unchanged_source(root.path(), before).unwrap();
        crate::raw::write(&root.path().join("xtask/build.rs"), b"fn main() {}\n").unwrap();
        super::unchanged_source(root.path(), before).unwrap_err();
    }

    #[test]
    fn changing_a_locked_dependency_or_required_corpus_invalidates_source_identity() {
        let root = fingerprint_fixture();
        let before = super::fingerprint(root.path()).unwrap();
        super::unchanged_source(root.path(), before).unwrap();
        crate::raw::write(&root.path().join("Cargo.lock"), b"modified").unwrap();
        super::unchanged_source(root.path(), before).unwrap_err();
        crate::raw::write(&root.path().join("Cargo.lock"), b"fixture").unwrap();
        let (target, seeds) = super::FUZZ_TARGETS.first().unwrap();
        let seed = seeds.first().unwrap();
        crate::raw::write(
            &root.path().join("fuzz/corpus").join(target).join(seed),
            b"modified",
        )
        .unwrap();
        super::unchanged_source(root.path(), before).unwrap_err();
    }

    #[test]
    fn a_missing_or_empty_required_seed_refuses_replay_before_starting_a_tool() {
        let root = tempfile::tempdir().unwrap();
        let (target, seeds) = super::FUZZ_TARGETS.first().unwrap();
        let seed = seeds.first().unwrap();
        super::replay_fuzz(root.path()).unwrap_err();
        super::replay_target(root.path(), target, &[*seed]).unwrap_err();
        let directory = root.path().join("fuzz/corpus").join(target);
        crate::raw::create_dir_all(&directory).unwrap();
        crate::raw::write(&directory.join(seed), b"").unwrap();
        super::replay_fuzz(root.path()).unwrap_err();
        super::replay_target(root.path(), target, &[*seed]).unwrap_err();
        super::replay_target(root.path(), target, &[]).unwrap_err();
    }

    fn inventory_fixture() -> (tempfile::TempDir, serde_json::Value) {
        let root = tempfile::tempdir().unwrap();
        for directory in ["verification", "crates/domyjob-core/src"] {
            crate::raw::create_dir_all(&root.path().join(directory)).unwrap();
        }
        crate::raw::write(
            &root.path().join("crates/domyjob-core/src/fixture.rs"),
            b"mod proofs { #[kani::proof] fn positive() {} #[kani::proof] fn negative() {} }",
        )
        .unwrap();
        crate::raw::write(
            &root.path().join("verification/native.rs"),
            b"const _: () = ();",
        )
        .unwrap();
        let manifest = serde_json::json!({
            "schema_version": super::SCHEMA_VERSION, "kani_version": crate::resource_proofs::KANI_VERSION,
            "quint_version": super::QUINT_VERSION, "apalache_version": super::APALACHE_VERSION,
            "harnesses":[{"name":"fixture::proofs::positive", "covers":1, "contract":"contract", "input_domain":"domain", "bound":"bound", "production":"fixture"}],
            "negatives":[{"source":null, "harness":"fixture::proofs::negative", "assertion":"NEGATIVE_FIXTURE"}],
            "boundaries":[{"source":"verification/native.rs", "reason":"native", "evidence":"command", "reconsider":"supported verifier"}]
        });
        (root, manifest)
    }

    fn write_manifest(root: &std::path::Path, manifest: &serde_json::Value) {
        crate::raw::write(&root.join(super::MANIFEST), manifest.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn standalone_positive_and_negative_sources_require_exact_harness_registration() {
        let (root, mut manifest) = inventory_fixture();
        let source = "verification/standalone.rs";
        crate::raw::write(
            &root.path().join(source),
            b"#[kani::proof] fn positive() {} #[kani::proof] fn negative() {}",
        )
        .unwrap();
        let harness = serde_json::json!({
            "source":source, "name":"positive", "covers":1,
            "contract":"actual source", "input_domain":"finite", "bound":"no loops", "production":"fixture"
        });
        manifest
            .get_mut("harnesses")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(harness.clone());
        manifest
            .get_mut("negatives")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "source":source, "harness":"negative", "assertion":"NEGATIVE_STANDALONE"
            }));
        write_manifest(root.path(), &manifest);
        super::inventory(root.path()).unwrap();
        let complete = manifest.clone();
        manifest
            .get_mut("harnesses")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .pop();
        write_manifest(root.path(), &manifest);
        assert!(
            super::inventory(root.path())
                .unwrap_err()
                .contains("standalone")
        );
        let mut duplicate = complete.clone();
        duplicate
            .get_mut("harnesses")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(harness);
        write_manifest(root.path(), &duplicate);
        assert!(
            super::inventory(root.path())
                .unwrap_err()
                .contains("duplicate")
        );
        write_manifest(root.path(), &complete);
        crate::raw::write(
            &root.path().join(source),
            b"#[kani::proof] fn positive() {}",
        )
        .unwrap();
        assert!(
            super::inventory(root.path())
                .unwrap_err()
                .contains("standalone")
        );
    }

    #[test]
    fn inventory_rejects_empty_duplicate_and_unregistered_obligations() {
        let (root, manifest) = inventory_fixture();
        write_manifest(root.path(), &manifest);
        super::inventory(root.path()).unwrap();
        let mut empty = manifest.clone();
        empty
            .get_mut("harnesses")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .clear();
        write_manifest(root.path(), &empty);
        super::inventory(root.path()).unwrap_err();
        let mut duplicate = manifest.clone();
        let harnesses = duplicate
            .get_mut("harnesses")
            .unwrap()
            .as_array_mut()
            .unwrap();
        let first = harnesses.first().unwrap().clone();
        harnesses.push(first);
        write_manifest(root.path(), &duplicate);
        super::inventory(root.path()).unwrap_err();
        write_manifest(root.path(), &manifest);
        crate::raw::write(
            &root.path().join("crates/domyjob-core/src/additional.rs"),
            b"#[kani::proof] fn unregistered() {}",
        )
        .unwrap();
        assert!(
            super::inventory(root.path())
                .unwrap_err()
                .contains("missing")
        );
    }

    #[test]
    fn inventory_rejects_a_stale_name_or_duplicate_negative_control() {
        let (root, mut manifest) = inventory_fixture();
        let original = manifest.clone();
        manifest
            .get_mut("harnesses")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .first_mut()
            .unwrap()
            .get_mut("name")
            .unwrap()
            .clone_from(&serde_json::json!("fixture::proofs::stale"));
        write_manifest(root.path(), &manifest);
        assert!(super::inventory(root.path()).unwrap_err().contains("stale"));
        let mut duplicate = original;
        let negatives = duplicate
            .get_mut("negatives")
            .unwrap()
            .as_array_mut()
            .unwrap();
        let first = negatives.first().unwrap().clone();
        negatives.push(first);
        write_manifest(root.path(), &duplicate);
        super::inventory(root.path()).unwrap_err();
    }

    #[test]
    fn a_trace_must_be_complete_and_match_the_actual_rust_transition() {
        let mut states = vec![
            serde_json::json!({"#meta":{"index":0},"phase":"absent","before":"absent","command":"init","refused":false}),
        ];
        for index in 1..=super::TRACE_STEPS {
            states.push(serde_json::json!({"#meta":{"index":index},"phase":"absent","before":"absent","command":"disconnect","refused":false}));
        }
        let trace = serde_json::json!({"#meta":{"format":"ITF","status":"ok"},"vars":["phase","before","command","refused"],"states":states});
        let mut parsed: Trace = domyjob_core::ingress::value(trace.clone()).unwrap();
        let mut actions = BTreeSet::new();
        replay(&parsed, &mut actions).unwrap();
        assert_eq!(actions, BTreeSet::from([Command::Disconnect]));
        parsed.states.pop();
        replay(&parsed, &mut actions).unwrap_err();
        let mut mismatched: Trace = domyjob_core::ingress::value(trace).unwrap();
        mismatched.states.get_mut(1).unwrap().phase = domyjob_core::job_protocol::State::Finished;
        replay(&mismatched, &mut actions).unwrap_err();
    }
}
