use std::path::{Path, PathBuf};

use domyjob_core::capture_policy::CapturePolicy;
use domyjob_core::{jsonc, mcp_clients};
use serde::{Deserialize, Serialize};

use super::ops::{self, OpsError};
use super::store::{LinkState, Store, StoreError};
use super::sync::{self, Ssh, SyncError};
use crate::layout;
use crate::lock::{OsLock, Probe};
use crate::platform::clock::Deadline;
use crate::platform::service::{self as manager, ServiceError};
use crate::platform::user_files::{self, UserFileError};
use crate::process::{Tool, ToolError, ToolOutput, run_tool};
use crate::state_io::{self, StateError};

const PEER_PIN_TIMEOUT_SECONDS: u64 = 900;
const INITIAL_SYNC_TIMEOUT_SECONDS: u64 = 120;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SetupError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error(transparent)]
    Ops(#[from] OpsError),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    File(#[from] UserFileError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
    #[error(transparent)]
    Process(#[from] crate::process::ProcessError),
    #[error("locating this program: {0}")]
    Io(#[from] std::io::Error),
    #[error("editing the OpenCode configuration: {0}")]
    Jsonc(#[from] jsonc::JsoncError),
    #[error("the program path is not UTF-8")]
    Path,
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("the service record is corrupt: {0}")]
    Record(#[from] domyjob_core::ingress::JsonError),
    #[error("encoding the service record failed: {0}")]
    Encode(#[from] serde_json::Error),
    #[error(transparent)]
    Hooks(#[from] domyjob_core::hook_clients::HookError),
    #[error("{client} MCP registration failed: {detail}")]
    Registration {
        client: &'static str,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Finding {
    pub(crate) area: &'static str,
    pub(crate) ok: bool,
    pub(crate) detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fix: Option<String>,
}

impl Finding {
    fn ok(area: &'static str, detail: impl Into<String>) -> Self {
        Self {
            area,
            ok: true,
            detail: detail.into(),
            fix: None,
        }
    }

    fn problem(area: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            area,
            ok: false,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installed {
    program: String,
}

fn installed(paths: &layout::Chat) -> Result<Option<Installed>, SetupError> {
    match state_io::read_bytes(&paths.service_record())? {
        Some(bytes) => Ok(Some(domyjob_core::ingress::json(
            &bytes,
            domyjob_core::wire::MAX_CONTROL_BYTES,
        )?)),
        None => Ok(None),
    }
}

fn stable_program() -> Result<PathBuf, SetupError> {
    let target = crate::platform::stable_program(&crate::identity::tag())?;
    user_files::install_executable(&std::env::current_exe()?, &target)?;
    crate::builds::pin(&target)?;
    Ok(target)
}

fn utf8(path: &Path) -> Result<&str, SetupError> {
    path.to_str().ok_or(SetupError::Path)
}

pub(crate) fn service_install(paths: &layout::Chat) -> Result<Finding, SetupError> {
    let program = stable_program()?;
    let place = manager::install(&program, &paths.service_log())?;
    let record = Installed {
        program: utf8(&program)?.to_owned(),
    };
    state_io::write_bytes(&paths.service_record(), &serde_json::to_vec(&record)?)?;
    Ok(Finding::ok(
        "service",
        format!("installed {} running {}", place.display(), record.program),
    ))
}

pub(crate) fn service_uninstall(paths: &layout::Chat) -> Result<Finding, SetupError> {
    let removed = manager::uninstall()?;
    if OsLock::probe(&paths.service_lock())? == Probe::Held
        && let Some(bytes) = state_io::read_bytes(&paths.service_pid())?
        && let Ok(Ok(pid)) = std::str::from_utf8(&bytes).map(|text| text.trim().parse::<u32>())
    {
        crate::process::terminate(pid)?;
    }
    state_io::remove_file(&paths.service_record())?;
    Ok(Finding::ok(
        "service",
        if removed {
            "uninstalled"
        } else {
            "was not installed"
        },
    ))
}

pub(crate) fn service_present(paths: &layout::Chat) -> Result<bool, SetupError> {
    Ok(installed(paths)?.is_some() || OsLock::probe(&paths.service_lock())? == Probe::Held)
}

pub(crate) fn service_status(paths: &layout::Chat) -> Result<Finding, SetupError> {
    let running = OsLock::probe(&paths.service_lock())? == Probe::Held;
    let current = crate::platform::stable_program(&crate::identity::tag())?;
    Ok(match (installed(paths)?, running) {
        (None, false) => Finding::problem(
            "service",
            "not installed; messages arrive only while a chat command runs",
            "domyjob chat service install",
        ),
        (None, true) => Finding::ok("service", "running outside a service manager"),
        (Some(record), _) if Path::new(&record.program) != current => Finding::problem(
            "service",
            format!("runs an older build: {}", record.program),
            "domyjob chat service install",
        ),
        (Some(_), false) => Finding::problem(
            "service",
            "installed but not running",
            "domyjob chat service install",
        ),
        (Some(record), true) => Finding::ok("service", format!("running {}", record.program)),
    })
}

fn tool_with_capture(
    program: &str,
    arguments: &[String],
    policy: CapturePolicy,
) -> Result<Option<ToolOutput>, SetupError> {
    match run_tool(&Tool::new(program, arguments).output(policy)) {
        Ok(output) => Ok(Some(output)),
        Err(ToolError::Missing(_)) => Ok(None),
        Err(error @ ToolError::Run { .. }) => Err(error.into()),
    }
}

fn opencode_config() -> Result<PathBuf, SetupError> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(base) => PathBuf::from(base),
        None => crate::platform::home()?.join(".config"),
    };
    let directory = base.join("opencode");
    let jsonc = directory.join("opencode.jsonc");
    Ok(if user_files::read(&jsonc)?.is_some() {
        jsonc
    } else {
        directory.join("opencode.json")
    })
}

fn codex_hooks_path() -> Result<PathBuf, SetupError> {
    let directory = match std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        Some(directory) => PathBuf::from(directory),
        None => crate::platform::home()?.join(".codex"),
    };
    Ok(directory.join("hooks.json"))
}

fn codex_hooks(program: &str, repair: bool, path: &Path) -> Result<Finding, SetupError> {
    let document = user_files::read(path)?.unwrap_or_default();
    let commands = crate::process::chat::codex_hook_commands(program);
    let wanted = domyjob_core::hook_clients::codex_merge(&document, &commands)?;
    let registered = wanted.as_str() == document;
    if !registered && repair {
        user_files::write(path, wanted.as_str().as_bytes())?;
    }
    Ok(if registered || repair {
        Finding::problem(
            "codex-hooks",
            format!(
                "needs-review: lifecycle hooks configured in {}; native trust is checked by Codex",
                path.display()
            ),
            "open Codex and use /hooks to review and trust the domyjob hooks",
        )
    } else {
        Finding::problem(
            "codex-hooks",
            "lifecycle hooks are missing or use an older build",
            "domyjob chat setup",
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpClient {
    Claude,
    Codex,
}

impl McpClient {
    const fn name(self) -> &'static str {
        match self {
            Self::Claude => mcp_clients::CLAUDE,
            Self::Codex => mcp_clients::CODEX,
        }
    }
    fn get(self) -> Vec<String> {
        match self {
            Self::Claude => mcp_clients::claude_get(),
            Self::Codex => mcp_clients::codex_get(),
        }
    }
    fn remove(self) -> Vec<String> {
        match self {
            Self::Claude => mcp_clients::claude_remove(),
            Self::Codex => mcp_clients::codex_remove(),
        }
    }
    fn add(self, program: &str) -> Vec<String> {
        match self {
            Self::Claude => mcp_clients::claude_add(program),
            Self::Codex => mcp_clients::codex_add(program),
        }
    }
    fn matches(self, output: &ToolOutput, program: &str) -> bool {
        output.success
            && match self {
                Self::Claude => mcp_clients::claude_registration_matches(&output.stdout, program),
                Self::Codex => mcp_clients::codex_registration_matches(&output.stdout, program),
            }
    }
}

fn client_registration(
    client: McpClient,
    program: &str,
    repair: bool,
    run: &mut impl FnMut(&str, &[String]) -> Result<Option<ToolOutput>, SetupError>,
) -> Result<Option<Finding>, SetupError> {
    let Some(output) = run(client.name(), &client.get())? else {
        return Ok(None);
    };
    let mut registered = client.matches(&output, program);
    if !registered && repair {
        run(client.name(), &client.remove())?;
        let added = run(client.name(), &client.add(program))?
            .ok_or_else(|| ToolError::Missing(client.name().to_owned()))?;
        if !added.success {
            return Err(SetupError::Registration {
                client: client.name(),
                detail: domyjob_core::domain::terminal_text(if added.stderr.trim().is_empty() {
                    added.stdout.trim()
                } else {
                    added.stderr.trim()
                }),
            });
        }
        let observed = run(client.name(), &client.get())?
            .ok_or_else(|| ToolError::Missing(client.name().to_owned()))?;
        registered = client.matches(&observed, program);
        if !registered {
            return Err(SetupError::Registration {
                client: client.name(),
                detail: "the CLI did not report the exact registration after adding it".to_owned(),
            });
        }
    }
    Ok(Some(if registered {
        Finding::ok(client.name(), "MCP server registered")
    } else {
        Finding::problem(
            client.name(),
            "the domyjob MCP server is missing or points elsewhere",
            "domyjob chat setup",
        )
    }))
}

fn clients(program: &str, repair: bool) -> Result<Vec<Finding>, SetupError> {
    let mut findings = Vec::new();
    let fix = "domyjob chat setup";
    for client in [McpClient::Claude, McpClient::Codex] {
        let mut invoke = |name: &str, arguments: &[String]| {
            let policy = if arguments == client.get() {
                CapturePolicy::Complete
            } else {
                CapturePolicy::Prefix
            };
            tool_with_capture(name, arguments, policy)
        };
        if let Some(finding) = client_registration(client, program, repair, &mut invoke)? {
            findings.push(finding);
            if client == McpClient::Codex {
                findings.push(codex_hooks(program, repair, &codex_hooks_path()?)?);
            }
        }
    }
    if crate::platform::find_program("opencode").is_some() {
        let path = opencode_config()?;
        let document = user_files::read(&path)?.unwrap_or_default();
        let wanted = jsonc::set_member(
            &document,
            &mcp_clients::OPENCODE_MEMBER,
            &mcp_clients::opencode_mcp_value(program),
        )?;
        let registered = wanted == document;
        if !registered && repair {
            user_files::write(&path, wanted.as_bytes())?;
        }
        findings.push(if registered || repair {
            Finding::ok(
                "opencode",
                format!("MCP server registered in {}", path.display()),
            )
        } else {
            Finding::problem(
                "opencode",
                "the domyjob MCP server is missing or points elsewhere",
                fix,
            )
        });
    }
    Ok(findings)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Steps {
    pub(crate) replace: bool,
    pub(crate) service: bool,
    pub(crate) clients: bool,
}

pub(crate) fn setup(machines: &[String], steps: Steps) -> Result<Vec<Finding>, SetupError> {
    let Steps {
        replace,
        service,
        clients: register,
    } = steps;
    let store = Store::open()?;
    let mut findings = Vec::new();
    for alias in machines {
        let origin = sync::identify(
            &mut Ssh::new(alias)?,
            Deadline::after_seconds(PEER_PIN_TIMEOUT_SECONDS),
        )?;
        store.pin(alias, &origin, replace)?;
        findings.push(Finding::ok(
            "peer",
            format!("{alias} is chat machine {origin}"),
        ));
    }
    ops::publish_machine(&store)?;
    if service {
        findings.push(service_install(store.paths())?);
    }
    if register {
        let program = stable_program()?;
        findings.extend(clients(utf8(&program)?, true)?);
    }
    let report = sync::sync(
        &store,
        None,
        Deadline::after_seconds(INITIAL_SYNC_TIMEOUT_SECONDS),
    )?;
    findings.extend(report.peers.into_iter().map(|peer| match peer.state {
        LinkState::Synced => Finding::ok("sync", format!("{} synchronized", peer.machine)),
        LinkState::Deferred | LinkState::Failed => Finding::problem(
            "sync",
            format!("{}: {}", peer.machine, peer.detail.unwrap_or_default()),
            format!("domyjob chat sync {}", peer.machine),
        ),
    }));
    Ok(findings)
}

fn logged_in(
    tool_name: &'static str,
    arguments: &[&str],
    credentials: bool,
) -> Result<Option<Finding>, SetupError> {
    let arguments: Vec<String> = arguments.iter().map(|word| (*word).to_owned()).collect();
    let policy = if credentials {
        CapturePolicy::Complete
    } else {
        CapturePolicy::Prefix
    };
    let Some(output) = tool_with_capture(tool_name, &arguments, policy)? else {
        return Ok(None);
    };
    Ok(Some(login_finding(tool_name, &output, credentials)))
}

fn login_finding(tool_name: &'static str, output: &ToolOutput, credentials: bool) -> Finding {
    let signed_in = output.success && (!credentials || !output.stdout.contains("0 credentials"));
    if signed_in {
        Finding::ok(tool_name, "signed in")
    } else {
        Finding::problem(
            tool_name,
            "not signed in",
            format!("sign in with the {tool_name} CLI"),
        )
    }
}

pub(crate) fn doctor() -> Result<Vec<Finding>, SetupError> {
    let mut findings = vec![service_status(&layout::State::here()?.chat())?];
    match Store::open() {
        Ok(store) => findings.extend(store_findings(&store)?),
        Err(StoreError::Format(found)) => findings.push(Finding::problem(
            "store",
            format!("written in format {found}, which this build cannot read"),
            "domyjob chat reset --yes",
        )),
        Err(StoreError::State(unsafe_file)) => findings.push(Finding::problem(
            "store",
            unsafe_file.to_string(),
            "domyjob chat reset --yes",
        )),
        Err(other) => return Err(other.into()),
    }
    let program = crate::platform::stable_program(&crate::identity::tag())?;
    findings.extend(clients(utf8(&program)?, false)?);
    for check in [
        logged_in("claude", &["auth", "status"], false)?,
        logged_in("codex", &["login", "status"], false)?,
        logged_in("opencode", &["auth", "list"], true)?,
    ]
    .into_iter()
    .flatten()
    {
        findings.push(check);
    }
    Ok(findings)
}

fn store_findings(store: &Store) -> Result<Vec<Finding>, SetupError> {
    let mut findings = Vec::new();
    for (alias, link) in store.links()? {
        findings.push(match link.state {
            LinkState::Synced => Finding::ok("peer", format!("{alias} synchronized")),
            LinkState::Deferred | LinkState::Failed => Finding::problem(
                "peer",
                format!("{alias}: {}", link.detail.unwrap_or_default()),
                format!("check `ssh {alias}`, then run `domyjob chat sync {alias}`"),
            ),
        });
    }
    if store.peers()?.is_empty() {
        findings.push(Finding::problem(
            "peer",
            "no peers are pinned",
            "domyjob chat setup MACHINE...",
        ));
    }
    findings.extend(agents(store)?);
    Ok(findings)
}

fn agents(store: &Store) -> Result<Vec<Finding>, SetupError> {
    let mut findings = Vec::new();
    let directory = store.read(super::store::directory)?;
    for (name, config) in store.read(super::store::local_agents)? {
        let agent = domyjob_core::chat::id::AgentId::new(name.clone(), store.origin().clone());
        let Some((card, _)) = directory.agents.get(&agent) else {
            continue;
        };
        if std::fs::metadata(&config.cwd).is_err() {
            findings.push(Finding::problem(
                "agent",
                format!("{name}: its directory {} is missing", config.cwd),
                format!("domyjob chat agent update {name} --cwd DIRECTORY"),
            ));
        }
        if card.description.is_none() {
            findings.push(Finding::problem(
                "agent",
                format!("{name} has no description, so others cannot tell what to ask it"),
                format!("domyjob chat profile --as {name} --description TEXT"),
            ));
        }
        if card.mode == domyjob_core::chat::card::Mode::Managed
            && crate::platform::find_program(card.tool.as_str()).is_none()
        {
            findings.push(Finding::problem(
                "agent",
                format!("{name} runs {}, which is not on PATH", card.tool.as_str()),
                format!("install {}", card.tool.as_str()),
            ));
        }
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::codex_hooks;
    use crate::platform::user_files;

    #[test]
    fn a_failed_credentials_probe_never_reports_signed_in() {
        let output = crate::process::ToolOutput {
            success: false,
            stdout: String::new(),
            stderr: "authentication store unavailable".to_owned(),
        };
        assert!(!super::login_finding("opencode", &output, true).ok);
    }

    #[test]
    fn credential_presence_still_requires_a_successful_probe() {
        for (success, stdout, expected) in [
            (false, "1 credentials", false),
            (true, "0 credentials", false),
            (true, "1 credentials", true),
        ] {
            let output = crate::process::ToolOutput {
                success,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            };
            assert_eq!(super::login_finding("opencode", &output, true).ok, expected);
        }
    }

    #[test]
    fn failed_mcp_additions_never_report_success_or_attempt_hook_setup() {
        for client in [super::McpClient::Claude, super::McpClient::Codex] {
            let operations =
                refused_registration(client, |_operation| crate::process::ToolOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "permission denied".to_owned(),
                });
            assert_eq!(operations, ["get", "remove", "add"]);
        }
    }

    #[test]
    fn successful_add_acknowledgments_need_the_exact_post_add_registration() {
        for client in [super::McpClient::Claude, super::McpClient::Codex] {
            let operations = refused_registration(client, |operation| crate::process::ToolOutput {
                success: operation == "add",
                stdout: String::new(),
                stderr: String::new(),
            });
            assert_eq!(operations, ["get", "remove", "add", "get"]);
        }
    }

    fn refused_registration(
        client: super::McpClient,
        mut output: impl FnMut(&str) -> crate::process::ToolOutput,
    ) -> Vec<String> {
        let mut operations = Vec::new();
        super::client_registration(client, "/new/domyjob", true, &mut |_name, arguments| {
            let operation = arguments.get(1).unwrap();
            operations.push(operation.clone());
            Ok(Some(output(operation)))
        })
        .unwrap_err();
        operations
    }

    #[test]
    fn only_an_observed_matching_mcp_registration_reports_success() {
        for client in [super::McpClient::Claude, super::McpClient::Codex] {
            let mut reads = 0_usize;
            let registration = match client {
                super::McpClient::Claude => {
                    "Scope: User config\nType: stdio\nCommand: /new/domyjob\nArgs: mcp\n"
                }
                super::McpClient::Codex => {
                    r#"{"enabled":true,"transport":{"type":"stdio","command":"/new/domyjob","args":["mcp"]}}"#
                }
            };
            let finding = super::client_registration(
                client,
                "/new/domyjob",
                true,
                &mut |_name, arguments| {
                    if arguments.get(1).is_some_and(|operation| operation == "get") {
                        reads = reads.saturating_add(1);
                        return Ok(Some(crate::process::ToolOutput {
                            success: true,
                            stdout: if reads > 1 {
                                registration.to_owned()
                            } else {
                                String::new()
                            },
                            stderr: String::new(),
                        }));
                    }
                    Ok(Some(crate::process::ToolOutput {
                        success: true,
                        stdout: String::new(),
                        stderr: String::new(),
                    }))
                },
            )
            .unwrap()
            .unwrap();
            assert!(finding.ok);
        }
    }

    #[test]
    fn installing_codex_hooks_preserves_user_commands_and_requires_native_review() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codex").join("hooks.json");
        user_files::write(
            &path,
            br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"owner-stop"}]}]}}"#,
        )
        .unwrap();
        let before = user_files::read(&path).unwrap().unwrap();
        let absent = codex_hooks("/new/domyjob", false, &path).unwrap();
        assert!(!absent.ok);
        assert_eq!(user_files::read(&path).unwrap().unwrap(), before);
        let installed = codex_hooks("/new/domyjob", true, &path).unwrap();
        assert!(!installed.ok);
        assert!(installed.detail.contains("needs-review"));
        assert!(installed.fix.unwrap().contains("/hooks"));
        let after = user_files::read(&path).unwrap().unwrap();
        assert!(after.contains("owner-stop"));
        assert!(after.contains("/new/domyjob"));
        codex_hooks("/new/domyjob", true, &path).unwrap();
        assert_eq!(user_files::read(&path).unwrap().unwrap(), after);
    }

    #[test]
    fn corrupt_codex_hooks_are_reported_and_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("hooks.json");
        user_files::write(&path, br#"{"hooks":{"Stop":[null]}}"#).unwrap();
        let original = user_files::read(&path).unwrap();
        codex_hooks("/new/domyjob", true, &path).unwrap_err();
        assert_eq!(user_files::read(&path).unwrap(), original);
    }
}
