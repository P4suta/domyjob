use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

use serde_json::{Map, Value};

const INPUT_LIMIT: u64 = 4_194_304;
const CHILD_TARGETS: [&str; 2] = ["target/ci-build", "target/ci-build-alt"];
const XTASK_CLIPPY: [&str; 8] = [
    "clippy",
    "--locked",
    "-p",
    "xtask",
    "--all-targets",
    "--",
    "-D",
    "warnings",
];

#[derive(Debug, thiserror::Error)]
pub enum CiError {
    #[error("CI input or output: {0}")]
    Io(#[from] std::io::Error),
    #[error("CI JSON: {0}")]
    Json(#[from] domyjob_core::ingress::JsonError),
    #[error("{0} exited with {1}")]
    Tool(String, ExitStatus),
    #[error("{0}")]
    Invalid(&'static str),
}

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "CI adapters expose bounded input, public output, and build-directory filesystem queries"
    )]

    use std::io::{Read, Write};
    pub(super) fn read(input: &mut impl Read, bytes: &mut Vec<u8>) -> std::io::Result<usize> {
        input.read_to_end(bytes)
    }

    pub(super) fn print(bytes: &[u8]) -> std::io::Result<()> {
        std::io::stdout().write_all(bytes)
    }

    pub(super) fn canonicalize(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        std::fs::canonicalize(path)
    }

    pub(super) fn metadata(path: &std::path::Path) -> std::io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(path)
    }

    #[cfg(all(test, unix))]
    pub(super) fn link_directory(
        target: &std::path::Path,
        link: &std::path::Path,
    ) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }
}

#[derive(Debug)]
struct ChildBuildDirectory {
    root: PathBuf,
    target: PathBuf,
}

impl ChildBuildDirectory {
    fn current(root: &Path) -> Result<Self, CiError> {
        Self::for_parent(root, &std::env::current_exe()?)
    }

    fn for_parent(root: &Path, parent: &Path) -> Result<Self, CiError> {
        let root = raw::canonicalize(root)?;
        let parent = raw::canonicalize(parent)?;
        if !raw::metadata(&root)?.is_dir() || !raw::metadata(&parent)?.is_file() {
            return Err(CiError::Invalid(
                "CI root and live executable must have their expected file types",
            ));
        }
        let cache_root = resolved_directory(&root.join("target"))?;
        if !cache_root.starts_with(&root) || cache_root == root {
            return Err(CiError::Invalid(
                "CI target cache must remain inside the checkout",
            ));
        }
        for relative in CHILD_TARGETS {
            let candidate = root.join(relative);
            let target = resolved_directory(&candidate)?;
            if !target.starts_with(&cache_root) || target == cache_root {
                return Err(CiError::Invalid(
                    "CI child target must remain inside the checkout's target cache",
                ));
            }
            if parent.starts_with(&target) {
                continue;
            }
            crate::raw::create_dir_all(&candidate)?;
            if raw::canonicalize(&candidate)? != target || !raw::metadata(&target)?.is_dir() {
                return Err(CiError::Invalid(
                    "CI child target changed while it was being validated",
                ));
            }
            return Ok(Self {
                root: native_command_directory(root)?,
                target: native_command_directory(target)?,
            });
        }
        Err(CiError::Invalid(
            "reserved CI child targets contain the live executable",
        ))
    }

    fn configure(&self, command: &mut Command) {
        command
            .current_dir(&self.root)
            .env("CARGO_TARGET_DIR", &self.target);
    }

    fn execute(&self, program: &str, arguments: &[&str]) -> Result<(), CiError> {
        let mut command = crate::raw::command(program);
        self.configure(&mut command);
        command.args(arguments);
        require_success(program, command.status()?)
    }
}

pub(crate) fn native_command_directory(canonical: PathBuf) -> Result<PathBuf, CiError> {
    if raw::canonicalize(&canonical)? != canonical || !raw::metadata(&canonical)?.is_dir() {
        return Err(CiError::Invalid(
            "native command directory requires an existing canonical directory",
        ));
    }
    if std::env::consts::OS != "windows" {
        return Ok(canonical);
    }
    let normal = windows_directory_shape(&canonical)?;
    if raw::canonicalize(&normal)? != canonical {
        return Err(CiError::Invalid(
            "CI command directory must resolve to its validated canonical directory",
        ));
    }
    Ok(normal)
}

fn windows_directory_shape(path: &Path) -> Result<PathBuf, CiError> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let mut normal = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
            Prefix::VerbatimUNC(server, share) => {
                let mut unc_prefix = std::ffi::OsString::from(r"\\");
                unc_prefix.push(server);
                unc_prefix.push(r"\");
                unc_prefix.push(share);
                unc_prefix.push(r"\");
                PathBuf::from(unc_prefix)
            }
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) | Prefix::UNC(_, _) | Prefix::Disk(_) => {
                return Err(CiError::Invalid(
                    "CI command directory has an unsupported prefix",
                ));
            }
        },
        _ => {
            return Err(CiError::Invalid(
                "CI command directory needs an absolute Windows prefix",
            ));
        }
    };
    if components.next() != Some(Component::RootDir) {
        return Err(CiError::Invalid(
            "CI command directory must have an absolute root",
        ));
    }
    for component in components {
        let Component::Normal(name) = component else {
            return Err(CiError::Invalid(
                "CI command directory has a non-normal component",
            ));
        };
        normal.push(name);
    }
    Ok(normal)
}

fn resolved_directory(path: &Path) -> Result<PathBuf, CiError> {
    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        match raw::metadata(ancestor) {
            Ok(_) => {
                let mut resolved = raw::canonicalize(ancestor)?;
                if !raw::metadata(&resolved)?.is_dir() {
                    return Err(CiError::Invalid(
                        "CI child target has a non-directory ancestor",
                    ));
                }
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or(CiError::Invalid("CI child target has no existing ancestor"))?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or(CiError::Invalid("CI child target has no existing ancestor"))?;
            }
            Err(error) => return Err(CiError::Io(error)),
        }
    }
}

fn bounded_read(input: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    raw::read(&mut input.take(INPUT_LIMIT.saturating_add(1)), &mut bytes)?;
    if u64::try_from(bytes.len()).is_ok_and(|length| length <= INPUT_LIMIT) {
        Ok(bytes)
    } else {
        Err(std::io::Error::other("CI input exceeds its byte limit"))
    }
}

fn capture(command: &mut Command) -> std::io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("Git stdout was not piped"))?;
    match bounded_read(stdout) {
        Ok(stdout) => Ok(Output {
            status: child.wait()?,
            stdout,
            stderr: Vec::new(),
        }),
        Err(error) => {
            let killed = child.kill();
            let waited = child.wait();
            killed?;
            waited?;
            Err(error)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Check {
    Docs,
    Workflows,
    Xtask,
    XtaskNative,
    WindowsContracts,
    Product,
    Dependencies,
    DependencyReview,
    Fuzz,
    CodeqlRust,
    CodeqlActions,
    Scorecard,
    Fleet,
}

const CHECKS: [Check; 13] = [
    Check::Docs,
    Check::Workflows,
    Check::Xtask,
    Check::XtaskNative,
    Check::WindowsContracts,
    Check::Product,
    Check::Dependencies,
    Check::DependencyReview,
    Check::Fuzz,
    Check::CodeqlRust,
    Check::CodeqlActions,
    Check::Scorecard,
    Check::Fleet,
];

impl Check {
    const fn name(self) -> &'static str {
        match self {
            Self::Docs => "docs",
            Self::Workflows => "workflows",
            Self::Xtask => "xtask",
            Self::XtaskNative => "xtask_native",
            Self::WindowsContracts => "windows_contracts",
            Self::Product => "product",
            Self::Dependencies => "dependencies",
            Self::DependencyReview => "dependency_review",
            Self::Fuzz => "fuzz",
            Self::CodeqlRust => "codeql_rust",
            Self::CodeqlActions => "codeql_actions",
            Self::Scorecard => "scorecard",
            Self::Fleet => "fleet",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Plan(BTreeSet<Check>);

impl Plan {
    fn docs() -> Self {
        Self(BTreeSet::from([Check::Docs]))
    }

    fn full() -> Self {
        Self(CHECKS.into_iter().collect())
    }

    fn has(&self, check: Check) -> bool {
        self.0.contains(&check)
    }

    fn workflows(&mut self) {
        self.0.extend([
            Check::Workflows,
            Check::Xtask,
            Check::DependencyReview,
            Check::CodeqlActions,
            Check::Scorecard,
        ]);
    }

    fn xtask(&mut self) {
        self.0
            .extend([Check::Workflows, Check::Xtask, Check::CodeqlRust]);
    }

    fn native_xtask(&mut self) {
        self.xtask();
        self.0.extend([Check::XtaskNative, Check::Fleet]);
    }

    fn windows_contracts(&mut self) {
        self.native_xtask();
        self.0.insert(Check::WindowsContracts);
    }

    fn json(&self) -> Value {
        let mut object = Map::from_iter([("schemaVersion".to_owned(), Value::from(1))]);
        object.extend(CHECKS.map(|check| (check.name().to_owned(), Value::from(self.has(check)))));
        Value::Object(object)
    }

    fn decode(text: &str) -> Result<Self, CiError> {
        let value: Value = domyjob_core::ingress::foreign_json(text)?;
        let object = value
            .as_object()
            .ok_or(CiError::Invalid("CI plan must be an object"))?;
        if object.len() != CHECKS.len().saturating_add(1)
            || object.get("schemaVersion") != Some(&Value::from(1))
        {
            return Err(CiError::Invalid("CI plan schema or flags are unsupported"));
        }
        let mut selected = BTreeSet::new();
        for check in CHECKS {
            match object.get(check.name()).and_then(Value::as_bool) {
                Some(true) => {
                    selected.insert(check);
                }
                Some(false) => {}
                None => return Err(CiError::Invalid("every CI plan flag must be a boolean")),
            }
        }
        let plan = Self(selected);
        let expected = if plan.has(Check::Product) {
            Self::full()
        } else {
            let mut expected = Self::docs();
            if plan.has(Check::CodeqlActions) {
                expected.workflows();
            }
            if plan.has(Check::CodeqlRust) {
                expected.xtask();
            }
            if plan.has(Check::XtaskNative) {
                expected.native_xtask();
            }
            if plan.has(Check::WindowsContracts) {
                expected.windows_contracts();
            }
            expected
        };
        if plan != expected {
            return Err(CiError::Invalid(
                "CI plan flags contradict the scope policy",
            ));
        }
        Ok(plan)
    }
}

fn classify(paths: &[String]) -> Plan {
    let mut plan = Plan::docs();
    for path in paths {
        if path.starts_with('/')
            || path.contains('\\')
            || path.split('/').any(|part| matches!(part, "" | "." | ".."))
            || path.split('/').skip(1).any(|part| part.starts_with('.'))
        {
            return Plan::full();
        }
        if matches!(path.as_str(), "README.md" | "CHANGELOG.md")
            || path.starts_with("docs/")
                && Path::new(path).extension().is_some_and(|ext| ext == "md")
            || matches!(path.as_str(), ".github/PULL_REQUEST_TEMPLATE.md")
        {
            continue;
        }
        if path.starts_with(".github/workflows/")
            && Path::new(path)
                .extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
            || path == ".github/actionlint.yaml"
        {
            plan.workflows();
        } else if path.starts_with("xtask/")
            && Path::new(path).extension().is_some_and(|ext| ext == "rs")
        {
            if matches!(
                path.as_str(),
                "xtask/src/windows_contracts.rs" | "xtask/src/lib.rs" | "xtask/src/main.rs"
            ) {
                plan.windows_contracts();
            } else if matches!(
                path.as_str(),
                "xtask/src/release.rs"
                    | "xtask/src/release_queue.rs"
                    | "xtask/src/release_orchestration.rs"
                    | "xtask/src/distribution.rs"
                    | "xtask/src/ci.rs"
            ) || path.starts_with("xtask/src/release/")
                || path.starts_with("xtask/tests/")
            {
                plan.native_xtask();
            } else {
                plan.xtask();
            }
        } else {
            return Plan::full();
        }
    }
    plan
}

fn git(root: &Path, arguments: &[&str]) -> Result<Vec<u8>, CiError> {
    let output = capture(
        crate::raw::command("git")
            .current_dir(root)
            .args(["-c", "core.fsmonitor=false"])
            .args(arguments),
    )?;
    if !output.status.success() {
        return Err(CiError::Tool("git".to_owned(), output.status));
    }
    Ok(output.stdout)
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().any(|byte| byte != b'0')
}

fn paths(bytes: &[u8]) -> Result<Vec<String>, CiError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let terminated = bytes
        .strip_suffix(&[0])
        .ok_or(CiError::Invalid("Git paths must be NUL terminated"))?;
    terminated
        .split(|byte| *byte == 0)
        .map(|path| {
            if path.is_empty() {
                return Err(CiError::Invalid("Git paths must not contain empty entries"));
            }
            String::from_utf8(path.to_vec())
                .map_err(|_error| CiError::Invalid("non-UTF-8 paths require the full scope"))
        })
        .collect()
}

fn changed(root: &Path, base: &str, head: &str) -> Result<Vec<String>, CiError> {
    if !oid(base) || !oid(head) {
        return Err(CiError::Invalid(
            "initial or invalid Git object IDs require the full scope",
        ));
    }
    git(root, &["merge-base", "--is-ancestor", base, head])?;
    let parents = git(root, &["rev-list", "--parents", "-n", "1", head])?;
    if parents
        .split(u8::is_ascii_whitespace)
        .filter(|part| !part.is_empty())
        .count()
        != 2
        || !git(
            root,
            &["rev-list", "--min-parents=2", &format!("{base}..{head}")],
        )?
        .is_empty()
    {
        return Err(CiError::Invalid(
            "merge or initial history requires the full scope",
        ));
    }
    paths(&git(
        root,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            base,
            head,
            "--",
        ],
    )?)
}

fn environment(name: &str) -> Result<String, CiError> {
    std::env::var(name)
        .map_err(|_error| CiError::Invalid("required CI environment input is missing"))
}

fn ci_plan(root: &Path) -> Result<Plan, CiError> {
    scope_inputs(
        root,
        ScopeInputs {
            event: &environment("CI_EVENT")?,
            base: &environment("CI_BASE")?,
            head: &environment("CI_HEAD")?,
            fork: &environment("CI_FORK")?,
        },
    )
}

#[derive(Clone, Copy, Debug)]
struct ScopeInputs<'a> {
    event: &'a str,
    base: &'a str,
    head: &'a str,
    fork: &'a str,
}

fn scope_inputs(root: &Path, inputs: ScopeInputs<'_>) -> Result<Plan, CiError> {
    if !matches!(inputs.event, "push" | "pull_request") || inputs.fork != "false" {
        return Err(CiError::Invalid(
            "scheduled, manual, or fork events require the full scope",
        ));
    }
    Ok(classify(&changed(root, inputs.base, inputs.head)?))
}

fn conservative(plan: Result<Plan, CiError>) -> Plan {
    match plan {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("ci: cannot narrow checks ({error}); selecting the full scope");
            Plan::full()
        }
    }
}

fn output(plan: &Plan) -> Result<(), CiError> {
    let json = plan.json().to_string();
    raw::print(format!("{json}\n").as_bytes())?;
    if let Some(path) = std::env::var_os("GITHUB_OUTPUT") {
        let mut lines = format!("plan={json}\n");
        for check in CHECKS {
            writeln!(lines, "{}={}", check.name(), plan.has(check))
                .map_err(|_error| CiError::Invalid("formatting CI output failed"))?;
        }
        let os = if plan.has(Check::Product) || plan.has(Check::XtaskNative) {
            serde_json::json!(["ubuntu-26.04", "macos-26", "windows-2025"])
        } else {
            serde_json::json!(["ubuntu-26.04"])
        };
        let mut languages = Vec::new();
        if plan.has(Check::CodeqlRust) {
            languages.push("rust");
        }
        if plan.has(Check::CodeqlActions) || languages.is_empty() {
            languages.push("actions");
        }
        writeln!(
            lines,
            "test_os={os}\nlanguages={}",
            serde_json::json!(languages)
        )
        .map_err(|_error| CiError::Invalid("formatting CI output failed"))?;
        crate::raw::append(Path::new(&path), lines.as_bytes())?;
    }
    Ok(())
}

fn execute(root: &Path, program: &str, arguments: &[&str]) -> Result<(), CiError> {
    if matches!(program, "cargo" | "mise") {
        return ChildBuildDirectory::current(root)?.execute(program, arguments);
    }
    let status = crate::raw::command(program)
        .current_dir(root)
        .args(arguments)
        .status()?;
    require_success(program, status)
}

fn require_success(program: &str, status: ExitStatus) -> Result<(), CiError> {
    if status.success() {
        Ok(())
    } else {
        Err(CiError::Tool(program.to_owned(), status))
    }
}

fn check(root: &Path, plan: &Plan) -> Result<(), CiError> {
    check_with(plan, &mut |program, arguments| {
        execute(root, program, arguments)
    })
}

fn static_checks(
    execute: &mut impl FnMut(&str, &[&str]) -> Result<(), CiError>,
) -> Result<(), CiError> {
    execute("mise", &["run", "fmt:check"])?;
    execute("cargo", &["xtask", "gates"])?;
    execute("typos", &[])?;
    execute("cargo", &["xtask", "workflows"])?;
    execute("jscpd", &["--config", ".jscpd.json", "--silent"])
}

fn check_with(
    plan: &Plan,
    execute: &mut impl FnMut(&str, &[&str]) -> Result<(), CiError>,
) -> Result<(), CiError> {
    if plan.has(Check::Product) {
        return execute("mise", &["run", "lint"]);
    }
    if plan.has(Check::Xtask) {
        static_checks(execute)?;
        return execute("cargo", &XTASK_CLIPPY);
    }
    execute("typos", &[])
}

fn xtask_clippy(root: &Path) -> Result<(), CiError> {
    execute(root, "cargo", &XTASK_CLIPPY)
}

fn xtask_tests(root: &Path, plan: &Plan) -> Result<(), CiError> {
    xtask_tests_with(plan, &mut |program, arguments| {
        execute(root, program, arguments)
    })
}

fn xtask_tests_with(
    plan: &Plan,
    execute: &mut impl FnMut(&str, &[&str]) -> Result<(), CiError>,
) -> Result<(), CiError> {
    if plan.has(Check::WindowsContracts) && std::env::consts::OS == "windows" {
        execute("cargo", &["xtask", "windows-contracts"])?;
    }
    execute("cargo", &["test", "--locked", "-p", "xtask"])
}

fn scoped_hook(
    plan: &Plan,
    execute: &mut impl FnMut(&str, &[&str]) -> Result<(), CiError>,
) -> Result<(), CiError> {
    check_with(plan, execute)?;
    if plan.has(Check::Xtask) {
        xtask_tests_with(plan, execute)?;
    }
    Ok(())
}

fn test(root: &Path, plan: &Plan) -> Result<(), CiError> {
    if plan.has(Check::Product) {
        execute(root, "mise", &["run", "clippy"])?;
        if std::env::consts::OS == "windows" {
            execute(root, "cargo", &["xtask", "windows-contracts"])?;
        }
        execute(root, "mise", &["run", "test"])
    } else if plan.has(Check::Xtask) {
        xtask_clippy(root)?;
        xtask_tests(root, plan)
    } else {
        Err(CiError::Invalid(
            "the selected CI scope has no Rust test job",
        ))
    }
}

fn remote_name(remote: &str) -> bool {
    !remote.is_empty()
        && !remote.starts_with(['-', '.'])
        && !remote.ends_with('.')
        && !remote.contains("..")
        && remote
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn new_branch_base(root: &Path, head: &str, remote: Option<&str>) -> Result<String, CiError> {
    let mut candidates = Vec::new();
    if let Some(remote) = remote {
        if !remote_name(remote) {
            return Err(CiError::Invalid("the push remote name is unsupported"));
        }
        candidates.push(format!("refs/remotes/{remote}/main"));
    }
    candidates.push("refs/heads/main".to_owned());
    for reference in candidates {
        match git(root, &["show-ref", "--verify", "--quiet", &reference]) {
            Ok(_) => {}
            Err(CiError::Tool(_, status)) if status.code() == Some(1) => continue,
            Err(error) => return Err(error),
        }
        let bytes = git(root, &["merge-base", &reference, head])?;
        let base = std::str::from_utf8(&bytes)
            .map_err(|_error| CiError::Invalid("the trusted main merge base is not UTF-8"))?
            .trim();
        if !oid(base) || base == head {
            return Err(CiError::Invalid(
                "an initial push or missing main range requires the full scope",
            ));
        }
        return Ok(base.to_owned());
    }
    Err(CiError::Invalid(
        "a new branch needs a trusted local main ref",
    ))
}

fn validate_ref(root: &Path, reference: &str) -> Result<(), CiError> {
    if !reference.starts_with("refs/heads/") && !reference.starts_with("refs/tags/") {
        return Err(CiError::Invalid("push ref namespaces are unsupported"));
    }
    git(root, &["check-ref-format", reference])?;
    Ok(())
}

fn push_plan(root: &Path, input: &[u8], push_remote: Option<&str>) -> Result<Plan, CiError> {
    if push_remote.is_some_and(|name| !remote_name(name)) {
        return Err(CiError::Invalid("the push remote name is unsupported"));
    }
    let text =
        std::str::from_utf8(input).map_err(|_error| CiError::Invalid("push refs must be UTF-8"))?;
    let mut changed_paths = BTreeSet::new();
    let mut count = 0_usize;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        let [local_ref, local, remote_ref, remote] = fields.as_slice() else {
            return Err(CiError::Invalid("each push ref must have four fields"));
        };
        validate_ref(root, local_ref)?;
        validate_ref(root, remote_ref)?;
        if !oid(local) {
            return Err(CiError::Invalid(
                "deleted or invalid local objects require the full scope",
            ));
        }
        let base = if matches!(remote.len(), 40 | 64) && remote.bytes().all(|byte| byte == b'0') {
            new_branch_base(root, local, push_remote)?
        } else {
            (*remote).to_owned()
        };
        changed_paths.extend(changed(root, &base, local)?);
        count = count.saturating_add(1);
    }
    if count == 0 {
        return Err(CiError::Invalid(
            "an absent push ref list requires the full scope",
        ));
    }
    changed_paths.extend(paths(&git(
        root,
        &["diff", "--no-renames", "--name-only", "-z", "HEAD", "--"],
    )?)?);
    changed_paths.extend(paths(&git(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?)?);
    Ok(classify(&changed_paths.into_iter().collect::<Vec<_>>()))
}

fn hook(root: &Path, remote: Option<&str>) -> Result<(), CiError> {
    let plan = conservative(
        bounded_read(std::io::stdin().lock())
            .map_err(CiError::Io)
            .and_then(|input| push_plan(root, &input, remote)),
    );
    output(&plan)?;
    if plan.has(Check::Product) {
        execute(root, "mise", &["run", "check"])?;
    } else {
        scoped_hook(&plan, &mut |program, arguments| {
            execute(root, program, arguments)
        })?;
    }
    if plan.has(Check::WindowsContracts) && !plan.has(Check::Product) {
        execute(root, "mise", &["run", "check:xtask:windows-contracts"])?;
    }
    if plan.has(Check::Fleet) {
        let task = if plan.has(Check::Product) {
            "check:fleet"
        } else {
            "check:xtask:fleet"
        };
        execute(root, "mise", &["run", task])?;
    }
    Ok(())
}

fn gate_jobs(workflow: &str, plan: &Plan) -> Result<Vec<(&'static str, bool)>, CiError> {
    let mut jobs = vec![("changes", true)];
    match workflow {
        "ci" => jobs.extend([
            ("lint", true),
            ("deny", plan.has(Check::Dependencies)),
            ("vet", plan.has(Check::Dependencies)),
            ("test", plan.has(Check::Product) || plan.has(Check::Xtask)),
            ("fuzz", plan.has(Check::Fuzz)),
        ]),
        "codeql" => jobs.push((
            "analyze",
            plan.has(Check::CodeqlRust) || plan.has(Check::CodeqlActions),
        )),
        "dependency-review" => jobs.push(("review", plan.has(Check::DependencyReview))),
        "scorecard" => jobs.push(("analysis", plan.has(Check::Scorecard))),
        _ => return Err(CiError::Invalid("the CI gate workflow is unsupported")),
    }
    Ok(jobs)
}

fn verify_gate(workflow: &str, plan_text: &str, needs_text: &str) -> Result<(), CiError> {
    let plan = Plan::decode(plan_text)?;
    let value: Value = domyjob_core::ingress::foreign_json(needs_text)?;
    let needs = value
        .as_object()
        .ok_or(CiError::Invalid("CI needs must be an object"))?;
    let jobs = gate_jobs(workflow, &plan)?;
    if needs.len() != jobs.len() {
        return Err(CiError::Invalid(
            "CI needs contains missing or unsupported jobs",
        ));
    }
    for (job, required) in jobs {
        let result = needs
            .get(job)
            .and_then(|job| job.get("result"))
            .and_then(Value::as_str)
            .ok_or(CiError::Invalid("a CI job result is missing"))?;
        if result != "success" && (required || result != "skipped") {
            return Err(CiError::Invalid(
                "a selected CI job did not succeed, or an unselected job failed",
            ));
        }
    }
    let actual_plan = needs
        .get("changes")
        .and_then(|job| job.get("outputs"))
        .and_then(|outputs| outputs.get("plan"))
        .and_then(Value::as_str)
        .ok_or(CiError::Invalid(
            "the successful changes job must supply its plan",
        ))?;
    if Plan::decode(actual_plan)? != plan {
        return Err(CiError::Invalid(
            "the gate plan differs from the changes job output",
        ));
    }
    Ok(())
}

pub fn run(root: &Path, words: &[&str]) -> Result<(), CiError> {
    match words {
        ["static"] => static_checks(&mut |program, arguments| execute(root, program, arguments)),
        ["plan"] => output(&conservative(ci_plan(root))),
        ["check"] => check(root, &Plan::decode(&environment("PLAN_JSON")?)?),
        ["test"] => test(root, &Plan::decode(&environment("PLAN_JSON")?)?),
        ["hook"] => hook(root, None),
        ["hook", remote] => hook(root, Some(remote)),
        ["gate", workflow] => verify_gate(
            workflow,
            &environment("PLAN_JSON")?,
            &environment("NEEDS_JSON")?,
        ),
        _ => Err(CiError::Invalid(
            "usage: cargo xtask ci static|plan|check|test|hook [REMOTE]|gate WORKFLOW",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value};

    use super::{
        CHILD_TARGETS, Check, ChildBuildDirectory, CiError, INPUT_LIMIT, Plan, ScopeInputs,
        bounded_read, capture, changed, check_with, classify, conservative, gate_jobs, git, oid,
        paths, push_plan, raw, scope_inputs, scoped_hook, verify_gate,
    };

    fn for_paths(paths: &[&str]) -> Plan {
        classify(
            &paths
                .iter()
                .map(|path| (*path).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn documentation_workflows_and_xtask_select_only_their_checks() {
        assert_eq!(for_paths(&["README.md", "docs/adr/001.md"]), Plan::docs());
        let workflow = for_paths(&[".github/workflows/release.yml"]);
        for selected in [
            Check::Docs,
            Check::Workflows,
            Check::Xtask,
            Check::DependencyReview,
            Check::CodeqlActions,
            Check::Scorecard,
        ] {
            assert!(workflow.has(selected));
        }
        for skipped in [
            Check::Product,
            Check::Dependencies,
            Check::Fuzz,
            Check::Fleet,
            Check::CodeqlRust,
        ] {
            assert!(!workflow.has(skipped));
        }
        let task = for_paths(&["xtask/src/comments.rs"]);
        assert!(task.has(Check::Xtask));
        assert!(task.has(Check::CodeqlRust));
        assert!(!task.has(Check::Product));
        assert!(!task.has(Check::Fleet));
    }

    struct CheckedCommands {
        result: Result<(), CiError>,
        calls: Vec<(String, Vec<String>)>,
    }

    fn checked_commands(plan: &Plan, hook: bool, fail: Option<&str>) -> CheckedCommands {
        let mut calls = Vec::new();
        let mut execute = |program: &str, arguments: &[&str]| {
            calls.push((
                program.to_owned(),
                arguments.iter().map(|word| (*word).to_owned()).collect(),
            ));
            if fail == Some(program) {
                Err(CiError::Invalid("static command failed"))
            } else {
                Ok(())
            }
        };
        let result = if hook {
            scoped_hook(plan, &mut execute)
        } else {
            check_with(plan, &mut execute)
        };
        CheckedCommands { result, calls }
    }

    #[test]
    fn scoped_ci_and_hook_share_static_checks_before_package_only_work() {
        for path in [
            "xtask/src/comments.rs",
            "xtask/src/release_orchestration.rs",
            ".github/workflows/release.yml",
        ] {
            let plan = for_paths(&[path]);
            let ci = checked_commands(&plan, false, None);
            let hook = checked_commands(&plan, true, None);
            ci.result.unwrap();
            hook.result.unwrap();
            assert_eq!(
                hook.calls.iter().take(ci.calls.len()).collect::<Vec<_>>(),
                ci.calls.iter().collect::<Vec<_>>()
            );
            for program in ["typos", "jscpd"] {
                assert!(
                    ci.calls
                        .iter()
                        .any(|(selected, _arguments)| selected == program)
                );
            }
            assert!(ci.calls.iter().any(
                |(program, arguments)| program == "mise" && arguments == &["run", "fmt:check"]
            ));
            assert!(
                ci.calls
                    .iter()
                    .any(|(program, arguments)| program == "cargo"
                        && arguments == &["xtask", "gates"])
            );
            assert!(
                ci.calls
                    .iter()
                    .any(|(program, arguments)| program == "cargo"
                        && arguments == &["xtask", "workflows"])
            );
            assert_eq!(
                hook.calls.last().unwrap().1,
                ["test", "--locked", "-p", "xtask"]
            );
            assert!(!hook.calls.iter().any(|(_program, arguments)| {
                arguments
                    .iter()
                    .any(|word| matches!(word.as_str(), "check:fleet" | "--workspace"))
            }));
        }
        let docs = checked_commands(&Plan::docs(), true, None);
        docs.result.unwrap();
        assert_eq!(docs.calls, [("typos".to_owned(), Vec::<String>::new())]);
    }

    #[test]
    fn static_failures_abort_ci_and_hook_before_clippy_tests_or_fleet() {
        let plan = for_paths(&["xtask/src/release_orchestration.rs"]);
        for program in ["mise", "typos", "jscpd"] {
            let ci = checked_commands(&plan, false, Some(program));
            let hook = checked_commands(&plan, true, Some(program));
            ci.result.unwrap_err();
            hook.result.unwrap_err();
            assert_eq!(ci.calls, hook.calls);
            assert_eq!(ci.calls.last().unwrap().0, program);
            assert!(!ci.calls.iter().any(|(_program, arguments)| {
                arguments
                    .iter()
                    .any(|word| matches!(word.as_str(), "clippy" | "test" | "check:xtask:fleet"))
            }));
        }
    }

    #[test]
    fn native_xtask_checks_limit_windows_compiler_contracts_to_their_owners() {
        let native = for_paths(&["xtask/src/release.rs"]);
        assert!(native.has(Check::XtaskNative));
        assert!(native.has(Check::Fleet));
        assert!(!native.has(Check::Product));
        assert!(!native.has(Check::WindowsContracts));
        for path in [
            "xtask/src/distribution.rs",
            "xtask/src/release_queue.rs",
            "xtask/src/release_orchestration.rs",
        ] {
            assert!(for_paths(&[path]).has(Check::XtaskNative));
            assert!(for_paths(&[path]).has(Check::Fleet));
        }
        let ci = for_paths(&["xtask/src/ci.rs"]);
        assert!(ci.has(Check::XtaskNative));
        assert!(!ci.has(Check::WindowsContracts));
        for path in [
            "xtask/src/windows_contracts.rs",
            "xtask/src/lib.rs",
            "xtask/src/main.rs",
        ] {
            let contracts = for_paths(&[path]);
            assert!(contracts.has(Check::WindowsContracts));
            assert!(contracts.has(Check::Fleet));
            assert!(!contracts.has(Check::Product));
        }
    }

    #[test]
    fn source_dependencies_tooling_unknown_and_hidden_paths_select_every_check() {
        for path in [
            "crates/domyjob/src/main.rs",
            "crates/domyjob-core/src/state.rs",
            "fuzz/fuzz_targets/wire.rs",
            "Cargo.lock",
            "xtask/Cargo.toml",
            "mise.toml",
            "lefthook.yml",
            ".cargo/config.toml",
            ".hidden/source.rs",
            "xtask/.hidden/source.rs",
            "docs/.hidden/source.md",
            "docs/source.rs",
            "unknown.md",
            "docs/../source.md",
            "/README.md",
            "docs\\README.md",
        ] {
            assert_eq!(for_paths(&[path]), Plan::full(), "{path}");
        }
        let union = for_paths(&[
            "docs/releasing.md",
            "xtask/src/ci.rs",
            ".github/workflows/ci.yml",
        ]);
        assert!(union.has(Check::CodeqlRust));
        assert!(union.has(Check::CodeqlActions));
        assert!(!union.has(Check::Product));
    }

    #[test]
    fn nul_paths_preserve_newlines_and_refuse_lossy_or_truncated_input() {
        assert_eq!(
            paths(b"docs/a\nb.md\0xtask/src/ci.rs\0").unwrap(),
            ["docs/a\nb.md", "xtask/src/ci.rs"]
        );
        paths(b"docs/a.md").unwrap_err();
        paths(b"\xff\0").unwrap_err();
        paths(b"docs/a.md\0\0").unwrap_err();
        assert!(oid(&"1".repeat(40)));
        assert!(oid(&"a".repeat(64)));
        assert!(!oid(&"0".repeat(40)));
        assert!(!oid("HEAD"));
        assert!(!oid("--help"));
    }

    #[test]
    fn plan_schema_refuses_missing_unknown_non_boolean_and_contradictory_flags() {
        for plan in [
            Plan::docs(),
            Plan::full(),
            for_paths(&["xtask/src/release.rs"]),
            for_paths(&["xtask/src/windows_contracts.rs"]),
            for_paths(&[".github/workflows/ci.yml"]),
        ] {
            assert_eq!(Plan::decode(&plan.json().to_string()).unwrap(), plan);
        }
        for invalid in ["null", "{}", "{\"schemaVersion\":2}"] {
            Plan::decode(invalid).unwrap_err();
        }
        let mut value = Plan::docs().json();
        *value.get_mut("fuzz").unwrap() = Value::Bool(true);
        Plan::decode(&value.to_string()).unwrap_err();
        value = Plan::docs().json();
        *value.get_mut("windows_contracts").unwrap() = Value::Bool(true);
        Plan::decode(&value.to_string()).unwrap_err();
        value = Plan::docs().json();
        *value.get_mut("docs").unwrap() = Value::String("true".to_owned());
        Plan::decode(&value.to_string()).unwrap_err();
        value = Plan::docs().json();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_owned(), Value::Bool(false));
        Plan::decode(&value.to_string()).unwrap_err();
    }

    fn needs(workflow: &str, plan: &Plan) -> Value {
        let mut object = Map::new();
        for (job, selected) in gate_jobs(workflow, plan).unwrap() {
            object.insert(job.to_owned(), serde_json::json!({"result": if selected {"success"} else {"skipped"}, "outputs": {}}));
        }
        let mut value = Value::Object(object);
        set(
            &mut value,
            &["changes", "outputs"],
            "plan",
            Value::String(plan.json().to_string()),
        );
        value
    }

    fn set(value: &mut Value, path: &[&str], key: &str, field: Value) {
        let mut object = value;
        for part in path {
            object = object.get_mut(*part).unwrap();
        }
        object
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), field);
    }

    #[test]
    fn gates_accept_only_explicitly_unselected_skips_and_the_successful_changes_plan() {
        for workflow in ["ci", "codeql", "dependency-review", "scorecard"] {
            let plan = Plan::docs();
            verify_gate(
                workflow,
                &plan.json().to_string(),
                &needs(workflow, &plan).to_string(),
            )
            .unwrap();
            rejected_gate(
                workflow,
                &plan,
                (&["changes"], "result", serde_json::json!("skipped")),
            );
            rejected_gate(
                workflow,
                &plan,
                (&[], "unknown", serde_json::json!({"result":"success"})),
            );
            rejected_gate(
                workflow,
                &plan,
                (
                    &["changes", "outputs"],
                    "plan",
                    Value::String(Plan::full().json().to_string()),
                ),
            );
        }
        rejected_gate(
            "ci",
            &Plan::full(),
            (&["test"], "result", serde_json::json!("skipped")),
        );
        rejected_gate(
            "ci",
            &Plan::docs(),
            (&["deny"], "result", serde_json::json!("failure")),
        );
    }

    fn rejected_gate(workflow: &str, plan: &Plan, replacement: (&[&str], &str, Value)) {
        let mut jobs = needs(workflow, plan);
        set(&mut jobs, replacement.0, replacement.1, replacement.2);
        verify_gate(workflow, &plan.json().to_string(), &jobs.to_string()).unwrap_err();
    }

    fn fixture_git(root: &Path, arguments: &[&str]) -> String {
        String::from_utf8(
            git(root, arguments).unwrap_or_else(|error| panic!("{arguments:?}: {error}")),
        )
        .unwrap()
        .trim()
        .to_owned()
    }

    fn commit(root: &Path) -> String {
        fixture_git(root, &["add", "--all"]);
        let parents = match git(root, &["rev-parse", "--verify", "HEAD"]) {
            Ok(bytes) => vec![String::from_utf8(bytes).unwrap().trim().to_owned()],
            Err(CiError::Tool(_, _)) => Vec::new(),
            Err(error) => panic!("fixture HEAD: {error}"),
        };
        let object = commit_object(root, &parents);
        fixture_git(root, &["update-ref", "HEAD", &object]);
        object
    }

    fn commit_object(root: &Path, parents: &[String]) -> String {
        let tree = fixture_git(root, &["write-tree"]);
        let mut args = vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            &tree,
            "-m",
            "test: fixture",
        ];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        fixture_git(root, &args)
    }

    fn fixture() -> tempfile::TempDir {
        fixture_branch("main")
    }

    fn fixture_branch(branch: &str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture_git(root, &["init", "--quiet", "--initial-branch", branch]);
        temp
    }

    fn documentation_base(branch: &str) -> (tempfile::TempDir, String) {
        let temp = fixture_branch(branch);
        crate::raw::write(&temp.path().join("README.md"), b"first").unwrap();
        let base = commit(temp.path());
        (temp, base)
    }

    fn edit_docs(root: &Path) -> String {
        crate::raw::write(&root.join("README.md"), b"updated").unwrap();
        crate::raw::write(&root.join("CHANGELOG.md"), b"new entry").unwrap();
        commit(root)
    }

    fn documentation_branch() -> (tempfile::TempDir, String, String) {
        let (temp, base) = documentation_base("main");
        fixture_git(temp.path(), &["switch", "--quiet", "-c", "docs"]);
        let head = edit_docs(temp.path());
        (temp, base, head)
    }

    #[test]
    fn actual_git_ranges_include_both_sides_of_renames_deletions_and_all_push_refs() {
        let temp = fixture();
        let root = temp.path();
        crate::raw::create_dir_all(&root.join("xtask/src")).unwrap();
        crate::raw::create_dir_all(&root.join("docs")).unwrap();
        crate::raw::write(&root.join("xtask/src/ci.rs"), b"source").unwrap();
        crate::raw::write(&root.join("README.md"), b"first").unwrap();
        let base = commit(root);
        fixture_git(root, &["mv", "xtask/src/ci.rs", "docs/ci.md"]);
        let renamed = commit(root);
        let changed_paths = changed(root, &base, &renamed).unwrap();
        assert!(changed_paths.iter().any(|path| path == "xtask/src/ci.rs"));
        assert!(changed_paths.iter().any(|path| path == "docs/ci.md"));
        assert!(classify(&changed_paths).has(Check::Xtask));
        fixture_git(root, &["rm", "docs/ci.md"]);
        let deleted = commit(root);
        assert_eq!(changed(root, &renamed, &deleted).unwrap(), ["docs/ci.md"]);
        let refs = format!(
            "refs/heads/a {renamed} refs/heads/a {base}\nrefs/heads/b {deleted} refs/heads/b {renamed}\n"
        );
        assert!(
            push_plan(root, refs.as_bytes(), None)
                .unwrap()
                .has(Check::Xtask)
        );
        push_plan(root, b"", None).unwrap_err();
        push_plan(root, b"incomplete\n", None).unwrap_err();
        changed(root, &deleted, &base).unwrap_err();
        changed(root, &"0".repeat(40), &deleted).unwrap_err();
    }

    #[test]
    fn documentation_pr_uses_exact_head_while_the_checkout_is_a_merge() {
        let (temp, base, head) = documentation_branch();
        let root = temp.path();
        fixture_git(root, &["switch", "--quiet", "main"]);
        crate::raw::create_dir_all(&root.join("crates")).unwrap();
        crate::raw::write(&root.join("crates/source.rs"), b"source").unwrap();
        let main = commit(root);
        let merge = commit_object(root, &[main, head.clone()]);
        fixture_git(root, &["update-ref", "HEAD", &merge]);
        let inputs = ScopeInputs {
            event: "pull_request",
            base: &base,
            head: &head,
            fork: "false",
        };
        assert_eq!(
            changed(root, &base, &head).unwrap(),
            ["CHANGELOG.md", "README.md"]
        );
        assert_eq!(scope_inputs(root, inputs).unwrap(), Plan::docs());
        assert_eq!(
            conservative(scope_inputs(
                root,
                ScopeInputs {
                    head: &merge,
                    ..inputs
                }
            )),
            Plan::full()
        );
        for unsupported in [
            ScopeInputs {
                fork: "true",
                ..inputs
            },
            ScopeInputs {
                event: "schedule",
                ..inputs
            },
            ScopeInputs {
                event: "workflow_dispatch",
                ..inputs
            },
        ] {
            assert_eq!(conservative(scope_inputs(root, unsupported)), Plan::full());
        }
    }

    #[test]
    fn new_documentation_branch_uses_trusted_local_main_and_validates_refs() {
        let (temp, base, head) = documentation_branch();
        let root = temp.path();
        let zero = "0".repeat(40);
        let refs = format!("refs/heads/docs {head} refs/heads/docs {zero}\n");
        assert_eq!(
            push_plan(root, refs.as_bytes(), None).unwrap(),
            Plan::docs()
        );
        let invalid = format!("refs/heads/../docs {head} refs/heads/docs {base}\n");
        assert_eq!(
            conservative(push_plan(root, invalid.as_bytes(), None)),
            Plan::full()
        );
        let missing = format!(
            "refs/heads/docs {} refs/heads/docs {base}\n",
            "1".repeat(40)
        );
        assert_eq!(
            conservative(push_plan(root, missing.as_bytes(), None)),
            Plan::full()
        );
        let initial = format!("refs/heads/main {base} refs/heads/main {zero}\n");
        assert_eq!(
            conservative(push_plan(root, initial.as_bytes(), None)),
            Plan::full()
        );
    }

    #[test]
    fn new_branch_can_use_tracking_main_without_local_main_and_missing_bases_fail_closed() {
        let (temp, base) = documentation_base("docs");
        let root = temp.path();
        let head = edit_docs(root);
        let refs = format!(
            "refs/heads/docs {head} refs/heads/docs {}\n",
            "0".repeat(40)
        );
        fixture_git(root, &["update-ref", "refs/remotes/origin/main", &base]);
        assert_eq!(
            push_plan(root, refs.as_bytes(), Some("origin")).unwrap(),
            Plan::docs()
        );
        assert_eq!(
            conservative(push_plan(root, refs.as_bytes(), None)),
            Plan::full()
        );
        for remote in ["-origin", "../origin", "origin;echo", "origin\nother"] {
            assert_eq!(
                conservative(push_plan(root, refs.as_bytes(), Some(remote))),
                Plan::full()
            );
        }
    }

    #[test]
    fn every_push_ref_and_local_source_change_participates_in_the_union() {
        let (temp, base) = documentation_base("main");
        let root = temp.path();
        let docs = edit_docs(root);
        crate::raw::create_dir_all(&root.join("xtask/src")).unwrap();
        crate::raw::write(&root.join("xtask/src/ci.rs"), b"source").unwrap();
        let task = commit(root);
        let refs = format!(
            "refs/heads/docs {docs} refs/heads/docs {base}\nrefs/heads/task {task} refs/heads/task {docs}\n"
        );
        let plan = push_plan(root, refs.as_bytes(), None).unwrap();
        assert!(plan.has(Check::Xtask));
        assert!(!plan.has(Check::Product));
        crate::raw::write(&root.join("unknown.rs"), b"untracked source").unwrap();
        assert_eq!(
            push_plan(root, refs.as_bytes(), None).unwrap(),
            Plan::full()
        );
        fixture_git(root, &["add", "unknown.rs"]);
        assert_eq!(
            push_plan(root, refs.as_bytes(), None).unwrap(),
            Plan::full()
        );
    }

    #[test]
    fn bounded_protocol_inputs_reject_excess_bytes() {
        assert_eq!(bounded_read(&b"bounded"[..]).unwrap(), b"bounded");
        bounded_read(std::io::repeat(b'x').take(INPUT_LIMIT.saturating_add(1))).unwrap_err();
    }

    fn live_parent(relative: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        let parent = temp.path().join(relative);
        crate::raw::create_dir_all(&root).unwrap();
        crate::raw::create_dir_all(parent.parent().unwrap()).unwrap();
        crate::raw::write(&parent, b"synthetic executable location").unwrap();
        (temp, root, parent)
    }

    #[cfg(unix)]
    fn link_directory(target: &Path, link: &Path) -> Result<(), CiError> {
        raw::link_directory(target, link).map_err(CiError::Io)
    }

    #[cfg(windows)]
    fn link_directory(target: &Path, link: &Path) -> Result<(), CiError> {
        let target = super::native_command_directory(raw::canonicalize(target)?)?;
        let parent = super::native_command_directory(raw::canonicalize(
            link.parent()
                .ok_or(CiError::Invalid("junction fixture has no parent"))?,
        )?)?;
        let status = crate::raw::command("cmd.exe")
            .current_dir(parent)
            .args(["/d", "/c", "mklink", "/j"])
            .arg(
                link.file_name()
                    .ok_or(CiError::Invalid("junction fixture has no file name"))?,
            )
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()?;
        super::require_success("cmd.exe", status)
    }

    #[test]
    fn child_builds_remain_disjoint_from_default_custom_and_reserved_parent_locations() {
        for (parent_path, selected) in [
            ("checkout/target/debug/xtask.exe", "target/ci-build"),
            ("custom-target/debug/xtask.exe", "target/ci-build"),
            (
                "checkout/target/ci-build/debug/xtask.exe",
                "target/ci-build-alt",
            ),
            (
                "checkout/target/ci-build-alt/debug/xtask.exe",
                "target/ci-build",
            ),
        ] {
            let (_temp, root, parent) = live_parent(parent_path);
            let context = ChildBuildDirectory::for_parent(&root, &parent).unwrap();
            let canonical_target = raw::canonicalize(&context.target).unwrap();
            assert_eq!(
                canonical_target,
                raw::canonicalize(&root.join(selected)).unwrap()
            );
            assert!(
                !raw::canonicalize(&parent)
                    .unwrap()
                    .starts_with(&canonical_target)
            );
            for program in ["cargo", "mise"] {
                let mut command = crate::raw::command(program);
                command.env("CARGO_TARGET_DIR", parent.parent().unwrap());
                context.configure(&mut command);
                assert_eq!(command.get_current_dir(), Some(context.root.as_path()));
                let forwarded = command
                    .get_envs()
                    .find(|(name, _value)| *name == "CARGO_TARGET_DIR")
                    .unwrap()
                    .1;
                assert_eq!(forwarded, Some(context.target.as_os_str()));
            }
        }
    }

    #[test]
    fn child_target_symlinks_cannot_alias_the_live_parent_or_escape_the_cache() {
        let (_temp, root, parent) = live_parent("checkout/target/debug/xtask.exe");
        link_directory(parent.parent().unwrap(), &root.join(CHILD_TARGETS[0])).unwrap();
        let context = ChildBuildDirectory::for_parent(&root, &parent).unwrap();
        assert_eq!(
            raw::canonicalize(&context.target).unwrap(),
            raw::canonicalize(&root.join(CHILD_TARGETS[1])).unwrap()
        );
        let (_other_temp, aliased_root, aliased_parent) =
            live_parent("checkout/target/debug/xtask.exe");
        for relative in CHILD_TARGETS {
            link_directory(
                aliased_parent.parent().unwrap(),
                &aliased_root.join(relative),
            )
            .unwrap();
        }
        ChildBuildDirectory::for_parent(&aliased_root, &aliased_parent).unwrap_err();
        let (escape_temp, escape_root, escape_parent) =
            live_parent("checkout/target/debug/xtask.exe");
        let outside = escape_temp.path().join("outside");
        crate::raw::create_dir_all(&outside).unwrap();
        link_directory(&outside, &escape_root.join(CHILD_TARGETS[0])).unwrap();
        ChildBuildDirectory::for_parent(&escape_root, &escape_parent).unwrap_err();
        raw::metadata(&escape_root.join(CHILD_TARGETS[1])).unwrap_err();
    }

    #[test]
    fn child_target_rejects_non_directory_and_dangling_ancestors_before_starting_commands() {
        let (_temp, root, parent) = live_parent("checkout/target/debug/xtask.exe");
        crate::raw::write(&root.join(CHILD_TARGETS[0]), b"not a directory").unwrap();
        ChildBuildDirectory::for_parent(&root, &parent).unwrap_err();
        let (dangling_temp, dangling_root, dangling_parent) =
            live_parent("checkout/target/debug/xtask.exe");
        let missing = tempfile::tempdir_in(dangling_temp.path()).unwrap();
        link_directory(missing.path(), &dangling_root.join(CHILD_TARGETS[0])).unwrap();
        missing.close().unwrap();
        ChildBuildDirectory::for_parent(&dangling_root, &dangling_parent).unwrap_err();
        super::native_command_directory(raw::canonicalize(&dangling_parent).unwrap()).unwrap_err();
        let mut noncanonical = raw::canonicalize(&dangling_root).unwrap().into_os_string();
        noncanonical.push(std::path::MAIN_SEPARATOR_STR);
        noncanonical.push("target");
        noncanonical.push(std::path::MAIN_SEPARATOR_STR);
        noncanonical.push("..");
        super::native_command_directory(PathBuf::from(noncanonical)).unwrap_err();
    }

    #[test]
    fn real_cargo_metadata_observes_the_validated_child_directory() {
        let (_temp, root, parent) = live_parent("custom-target/debug/xtask.exe");
        let root = root.join("space 日本語");
        crate::raw::create_dir_all(&root.join("src")).unwrap();
        crate::raw::write(&root.join("src/lib.rs"), b"").unwrap();
        crate::raw::write(&root.join("Cargo.toml"), b"[package]\nname = \"ci-target-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n").unwrap();
        crate::raw::write(
            &root.join("Cargo.lock"),
            b"version = 4\n[[package]]\nname = \"ci-target-fixture\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        let context = ChildBuildDirectory::for_parent(&root, &parent).unwrap();
        let mut command = crate::raw::command("cargo");
        command.env("CARGO_TARGET_DIR", parent.parent().unwrap());
        context.configure(&mut command);
        command.args([
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ]);
        let result = capture(&mut command).unwrap();
        assert!(result.status.success());
        let json: Value =
            domyjob_core::ingress::foreign_json(std::str::from_utf8(&result.stdout).unwrap())
                .unwrap();
        let target = json.get("target_directory").unwrap().as_str().unwrap();
        assert_eq!(
            raw::canonicalize(Path::new(target)).unwrap(),
            raw::canonicalize(&context.target).unwrap()
        );
        if std::env::consts::OS == "windows" {
            for directory in [&context.root, &context.target] {
                assert!(matches!(
                    directory.components().next(),
                    Some(std::path::Component::Prefix(prefix))
                        if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::UNC(_, _))
                ));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_directory_shapes_preserve_disk_unc_and_literal_components() {
        for (verbatim, normal) in [
            (r"\\?\C:\space 日本語\ci-build", r"C:\space 日本語\ci-build"),
            (
                r"\\?\UNC\server\share\space 日本語\ci-build",
                r"\\server\share\space 日本語\ci-build",
            ),
        ] {
            assert_eq!(
                super::windows_directory_shape(Path::new(verbatim)).unwrap(),
                PathBuf::from(normal)
            );
        }
        for unsupported in [
            r"\\.\COM1",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\ci-build",
            r"C:relative",
            r"relative\ci-build",
            r"\\?\C:\parent\..\ci-build",
        ] {
            super::windows_directory_shape(Path::new(unsupported)).unwrap_err();
        }
    }
}
