use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use serde_json::{Value, json};

const TARGET: &str = "x86_64-pc-windows-msvc";
const CAPTURE_LIMIT: u64 = 16 * 1024 * 1024;
const JSON_LIMIT: usize = 1024 * 1024;
const CATEGORIES: &[(&str, &[&str])] = &[
    ("resource_kind", &["E0308"]),
    ("event_access", &["E0599"]),
    ("process_access", &["E0599"]),
    ("configure_before_assign", &["E0599"]),
    ("watch_before_resume", &["E0599"]),
    ("descriptor_lifetime", &["E0597"]),
    ("checked_construction", &["E0599", "E0603"]),
];

fn validate_manifest(manifest: &Value) -> Result<(), String> {
    let sources = json!({
        "@KERNEL@": "crates/domyjob/src/process/windows/kernel.rs",
        "@DESCRIPTOR@": "crates/domyjob/src/platform/windows_acl/descriptor.rs"
    });
    if manifest.get("schema").and_then(Value::as_u64) != Some(1)
        || manifest.get("sdk_execution").and_then(Value::as_bool) != Some(false)
        || manifest.get("actual_source_placeholders") != Some(&sources)
    {
        return Err("unsupported compiler-contract manifest or leaf source paths".to_owned());
    }
    let fixtures = array(manifest, "fixtures")?;
    let mut seen = BTreeSet::new();
    for fixture in fixtures {
        let category = text(fixture, "category")?;
        let (_, expected) = CATEGORIES
            .iter()
            .find(|(name, _)| *name == category)
            .ok_or("unknown Windows capability category")?;
        if !seen.insert(category)
            || text(fixture, "source")? != format!("{category}.rs.fail")
            || text(fixture, "passing_control")? != "control.rs.pass"
            || fixture
                .get("require_primary_fixture_span")
                .and_then(Value::as_bool)
                != Some(true)
            || fixture.get("expected_error_codes") != Some(&json!(expected))
        {
            return Err(format!("{category}: compiler contract inventory changed"));
        }
        if category == "descriptor_lifetime"
            && fixture
                .pointer("/expected_min_diagnostics/E0597")
                .and_then(Value::as_u64)
                != Some(2)
        {
            return Err("both ACL and SID lifetime rejections are required".to_owned());
        }
    }
    if seen.len() != CATEGORIES.len() {
        return Err("expected every Windows capability category exactly once".to_owned());
    }
    Ok(())
}

struct Capture {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn bounded_file(mut file: File) -> Result<Vec<u8>, String> {
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if length > CAPTURE_LIMIT {
        return Err("compiler output exceeded 16 MiB".to_owned());
    }
    let mut bytes = vec![0; usize::try_from(length).map_err(|error| error.to_string())?];
    file.rewind().map_err(|error| error.to_string())?;
    file.read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn capture(command: &mut Command) -> Result<Capture, String> {
    let stdout = tempfile::tempfile().map_err(|error| error.to_string())?;
    let stderr = tempfile::tempfile().map_err(|error| error.to_string())?;
    command.stdout(stdout.try_clone().map_err(|error| error.to_string())?);
    command.stderr(stderr.try_clone().map_err(|error| error.to_string())?);
    let status = command.status().map_err(|error| error.to_string())?;
    Ok(Capture {
        status,
        stdout: bounded_file(stdout)?,
        stderr: bounded_file(stderr)?,
    })
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn decode(bytes: &[u8], limit: usize) -> Result<Value, String> {
    domyjob_core::ingress::json(bytes, limit).map_err(|error| error.to_string())
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("missing {key}"))
}

fn successful(capture: &Capture, operation: &str) -> Result<(), String> {
    if capture.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{operation} failed with {}: {}",
            capture.status,
            String::from_utf8_lossy(
                capture
                    .stderr
                    .get(..capture.stderr.len().min(4096))
                    .ok_or("invalid compiler output length")?
            )
        ))
    }
}

fn dependencies(metadata: &Value) -> Result<BTreeMap<String, String>, String> {
    let packages = array(metadata, "packages")?;
    let package = packages
        .iter()
        .find(|package| package.get("name").and_then(Value::as_str) == Some("domyjob"))
        .ok_or("missing domyjob package")?;
    let package_id = text(package, "id")?;
    let nodes = metadata
        .pointer("/resolve/nodes")
        .and_then(Value::as_array)
        .ok_or("missing Cargo resolution")?;
    let node = nodes
        .iter()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(package_id))
        .ok_or("missing domyjob dependency node")?;
    let deps = array(node, "deps")?;
    ["windows_sys", "windows_spawn"]
        .into_iter()
        .map(|name| {
            let dep = deps
                .iter()
                .find(|dep| dep.get("name").and_then(Value::as_str) == Some(name))
                .ok_or_else(|| format!("missing {name}"))?;
            Ok((name.to_owned(), text(dep, "pkg")?.to_owned()))
        })
        .collect()
}

fn artifacts(
    bytes: &[u8],
    dependencies: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut found = BTreeMap::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message = decode(line, JSON_LIMIT)?;
        if text(&message, "reason")? != "compiler-artifact" {
            continue;
        }
        for (name, package) in dependencies {
            if text(&message, "package_id")? != package
                || message.pointer("/target/name").and_then(Value::as_str) != Some(name.as_str())
            {
                continue;
            }
            for filename in array(&message, "filenames")? {
                let path = PathBuf::from(filename.as_str().ok_or("invalid artifact filename")?);
                if path
                    .extension()
                    .is_some_and(|extension| extension == "rmeta")
                    && found.insert(name.clone(), path).is_some()
                {
                    return Err(format!("ambiguous metadata for {name}"));
                }
            }
        }
    }
    if found.len() != dependencies.len() {
        return Err("Cargo did not emit the actual Windows dependency metadata".to_owned());
    }
    Ok(found)
}

fn fixture_codes(bytes: &[u8], source: &Path) -> Result<BTreeMap<String, Vec<u64>>, String> {
    let mut codes = BTreeMap::<String, Vec<u64>>::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let diagnostic = decode(line, JSON_LIMIT)?;
        if text(&diagnostic, "level")? != "error" {
            continue;
        }
        let spans = array(&diagnostic, "spans")?;
        if let Some(code) = diagnostic.pointer("/code/code").and_then(Value::as_str) {
            for span in spans {
                if span.get("is_primary").and_then(Value::as_bool) == Some(true)
                    && span
                        .get("file_name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| Path::new(name) == source)
                {
                    let primary_line = span
                        .get("line_start")
                        .and_then(Value::as_u64)
                        .ok_or("missing primary diagnostic line")?;
                    codes.entry(code.to_owned()).or_default().push(primary_line);
                    break;
                }
            }
        }
    }
    Ok(codes)
}

fn require_diagnostics(fixture: &Value, codes: &BTreeMap<String, Vec<u64>>) -> Result<(), String> {
    let category = text(fixture, "category")?;
    for code in array(fixture, "expected_error_codes")? {
        let code = code.as_str().ok_or("invalid compiler code")?;
        let range = fixture
            .get("expected_primary_line_ranges")
            .and_then(|ranges| ranges.get(code))
            .ok_or("missing expected diagnostic range")?;
        let start = range
            .get("start")
            .and_then(Value::as_u64)
            .ok_or("missing diagnostic range start")?;
        let end = range
            .get("end")
            .and_then(Value::as_u64)
            .ok_or("missing diagnostic range end")?;
        let minimum = fixture
            .get("expected_min_diagnostics")
            .and_then(|counts| counts.get(code))
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let lines = codes.get(code).map_or(&[][..], Vec::as_slice);
        let count = lines
            .iter()
            .filter(|line| (start..=end).contains(line))
            .count();
        if u64::try_from(count).map_err(|error| error.to_string())? < minimum || minimum == 0 {
            return Err(format!(
                "{category}: missing {code} in its expected primary fixture range"
            ));
        }
        if category == "descriptor_lifetime" && ![13, 22].iter().all(|line| lines.contains(line)) {
            return Err(
                "both ACL and SID lifetime sites must be rejected independently".to_owned(),
            );
        }
    }
    Ok(())
}

struct Contracts {
    root: PathBuf,
    fixtures: PathBuf,
    evidence: PathBuf,
    metadata: BTreeMap<String, PathBuf>,
    source_paths: BTreeMap<String, String>,
}

impl Contracts {
    fn source_digests(&self) -> Result<BTreeMap<String, String>, String> {
        self.source_paths
            .values()
            .map(|path| {
                let bytes = crate::raw::read_to_string(Path::new(path))
                    .map_err(|error| error.to_string())?;
                Ok((
                    path.clone(),
                    blake3::hash(bytes.as_bytes()).to_hex().to_string(),
                ))
            })
            .collect()
    }

    fn compile(&self, category: &str, template: &str) -> Result<(Capture, PathBuf), String> {
        let mut content = crate::raw::read_to_string(&self.fixtures.join(template))
            .map_err(|error| error.to_string())?;
        for (placeholder, path) in &self.source_paths {
            content = content.replace(placeholder, path);
        }
        let source = self.evidence.join(format!("{category}.rs"));
        crate::raw::write(&source, content.as_bytes()).map_err(|error| error.to_string())?;
        let mut command = crate::raw::command("rustc");
        command
            .current_dir(&self.root)
            .args([
                "--crate-name",
                "windows_capability_contract",
                "--crate-type",
                "lib",
                "--edition",
                "2024",
                "--target",
                TARGET,
                "--emit",
                "metadata",
                "--error-format",
                "json",
                "--out-dir",
            ])
            .arg(&self.evidence);
        for (name, path) in &self.metadata {
            command
                .arg("--extern")
                .arg(format!("{name}={}", path.display()));
            command.arg("-L").arg(format!(
                "dependency={}",
                path.parent().ok_or("metadata has no parent")?.display()
            ));
        }
        command.arg(&source);
        let result = capture(&mut command)?;
        crate::raw::write(
            &self.evidence.join(format!("{category}.jsonl")),
            &result.stderr,
        )
        .map_err(|error| error.to_string())?;
        Ok((result, source))
    }

    fn reject(&self, fixture: &Value) -> Result<Value, String> {
        let category = text(fixture, "category")?;
        let (result, source) = self.compile(category, text(fixture, "source")?)?;
        if result.status.success() {
            return Err(format!(
                "{category}: prohibited program compiled successfully"
            ));
        }
        let codes = fixture_codes(&result.stderr, &source)?;
        require_diagnostics(fixture, &codes)
            .map_err(|error| format!("{error}; see {}", source.display()))?;
        Ok(
            json!({"category":category,"source":source,"primary_error_lines":codes,"exit":result.status.code()}),
        )
    }
}

fn prepare(root: &Path, manifest: &Value) -> Result<Contracts, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let mut query = crate::raw::command("cargo");
    query
        .current_dir(&root)
        .args(["metadata", "--locked", "--format-version", "1"]);
    let result = capture(&mut query)?;
    successful(&result, "Cargo metadata")?;
    let graph = decode(
        &result.stdout,
        usize::try_from(CAPTURE_LIMIT).map_err(|error| error.to_string())?,
    )?;
    let dependencies = dependencies(&graph)?;
    let evidence = PathBuf::from(text(&graph, "target_directory")?).join("windows-contracts");
    crate::raw::create_dir_all(&evidence).map_err(|error| error.to_string())?;
    crate::raw::write(&evidence.join("cargo-metadata.json"), &result.stdout)
        .map_err(|error| error.to_string())?;
    let mut cargo = crate::raw::command("cargo");
    cargo
        .current_dir(&root)
        .env("CARGO_FEATURE_PURE", "1")
        .args([
            "check",
            "--locked",
            "-p",
            "domyjob",
            "--bin",
            "domyjob",
            "--target",
            TARGET,
            "--message-format",
            "json",
        ]);
    let checked = capture(&mut cargo)?;
    crate::raw::write(&evidence.join("cargo-check.jsonl"), &checked.stdout)
        .map_err(|error| error.to_string())?;
    crate::raw::write(&evidence.join("cargo-check.stderr.txt"), &checked.stderr)
        .map_err(|error| error.to_string())?;
    successful(&checked, "Cargo Windows check")?;
    let metadata = artifacts(&checked.stdout, &dependencies)?;
    let mut source_paths = BTreeMap::new();
    for (placeholder, path) in manifest
        .get("actual_source_placeholders")
        .and_then(Value::as_object)
        .ok_or("missing actual source paths")?
    {
        let path = root
            .join(path.as_str().ok_or("invalid source path")?)
            .canonicalize()
            .map_err(|error| error.to_string())?;
        source_paths.insert(placeholder.clone(), path.to_string_lossy().into_owned());
    }
    Ok(Contracts {
        fixtures: root.join("crates/domyjob/tests/compile/windows-capabilities"),
        root,
        evidence,
        metadata,
        source_paths,
    })
}

pub fn run(root: &Path) -> Result<(), String> {
    let manifest_path =
        root.join("crates/domyjob/tests/compile/windows-capabilities/manifest.json");
    let manifest = decode(
        crate::raw::read_to_string(&manifest_path)
            .map_err(|error| error.to_string())?
            .as_bytes(),
        JSON_LIMIT,
    )?;
    validate_manifest(&manifest)?;
    let contracts = prepare(root, &manifest)?;
    let sources = contracts.source_digests()?;
    let (control, _) = contracts.compile("control", "control.rs.pass")?;
    successful(&control, "Windows passing compiler control")?;
    let fixtures = array(&manifest, "fixtures")?;
    let results = fixtures
        .iter()
        .map(|fixture| contracts.reject(fixture))
        .collect::<Result<Vec<_>, _>>()?;
    if contracts.source_digests()? != sources {
        return Err("Windows leaf sources changed during compiler verification".to_owned());
    }
    let receipt = json!({"schema":1,"target":TARGET,"sdk_execution":false,"passing_control":true,"unchanged_sources_blake3":sources,"dependency_metadata":contracts.metadata,"rejections":results});
    crate::raw::write(
        &contracts.evidence.join("receipt.json"),
        &serde_json::to_vec_pretty(&receipt).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    eprintln!(
        "windows-contracts: seven prohibited categories rejected; passing control compiled; {}",
        contracts.evidence.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{dependencies, fixture_codes, require_diagnostics, validate_manifest};
    use serde_json::json;

    #[test]
    fn compiler_rejection_requires_a_primary_span_in_the_actual_fixture() {
        let source = std::path::Path::new("contract.rs");
        for (file, primary, level, matched) in [
            ("contract.rs", true, "error", true),
            ("leaf.rs", true, "error", false),
            ("contract.rs", false, "error", false),
            ("contract.rs", true, "warning", false),
        ] {
            let diagnostic = json!({"level":level,"code":{"code":"E0308"},"spans":[{"file_name":file,"is_primary":primary,"line_start":11}]});
            let codes = fixture_codes(&serde_json::to_vec(&diagnostic).unwrap(), source).unwrap();
            assert_eq!(codes.contains_key("E0308"), matched);
        }
    }

    #[test]
    fn metadata_uses_the_packages_resolved_for_domyjob() {
        let metadata = json!({"packages":[{"name":"domyjob","id":"product"}],"resolve":{"nodes":[{"id":"product","deps":[{"name":"windows_sys","pkg":"sys-new"},{"name":"windows_spawn","pkg":"spawn"}]},{"id":"other","deps":[{"name":"windows_sys","pkg":"sys-old"}]}]}});
        assert_eq!(
            dependencies(&metadata).unwrap().get("windows_sys").unwrap(),
            "sys-new"
        );
        let _error = dependencies(&json!({})).unwrap_err();
    }

    #[test]
    fn compiler_inventory_cannot_drop_a_category_or_substitute_the_leaf_sources() {
        let manifest = super::decode(
            include_bytes!("../../crates/domyjob/tests/compile/windows-capabilities/manifest.json"),
            super::JSON_LIMIT,
        )
        .unwrap();
        validate_manifest(&manifest).unwrap();
        let changes = [
            (
                "/fixtures/1",
                manifest.pointer("/fixtures/0").unwrap().clone(),
            ),
            ("/actual_source_placeholders/@KERNEL@", json!("stub.rs")),
            ("/fixtures/0/expected_error_codes", json!([])),
            ("/fixtures/0/require_primary_fixture_span", json!(false)),
            ("/fixtures/5/expected_min_diagnostics/E0597", json!(1)),
        ];
        for (path, value) in changes {
            let mut candidate = manifest.clone();
            *candidate.pointer_mut(path).unwrap() = value;
            assert!(validate_manifest(&candidate).is_err(), "{path}");
        }
    }

    #[test]
    fn duplicate_primary_spans_cannot_replace_independent_acl_and_sid_rejections() {
        let source = std::path::Path::new("contract.rs");
        let diagnostic = json!({"level":"error","code":{"code":"E0597"},"spans":[{"file_name":"contract.rs","is_primary":true,"line_start":13},{"file_name":"contract.rs","is_primary":true,"line_start":13}]});
        let codes = fixture_codes(&serde_json::to_vec(&diagnostic).unwrap(), source).unwrap();
        assert_eq!(codes.get("E0597").unwrap(), &[13]);
        let fixture = json!({"category":"descriptor_lifetime","expected_error_codes":["E0597"],"expected_primary_line_ranges":{"E0597":{"start":13,"end":22}},"expected_min_diagnostics":{"E0597":2}});
        assert!(require_diagnostics(&fixture, &codes).is_err());
        let repeated = std::collections::BTreeMap::from([("E0597".to_owned(), vec![13, 13])]);
        assert!(require_diagnostics(&fixture, &repeated).is_err());
        let independent = std::collections::BTreeMap::from([("E0597".to_owned(), vec![13, 22])]);
        require_diagnostics(&fixture, &independent).unwrap();
    }
}
