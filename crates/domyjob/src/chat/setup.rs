//! Preparing this machine for chat and diagnosing it.
//!
//! Setup pins peers, installs this build at a stable path, starts the background service,
//! and registers the MCP server with every installed AI client; each step is idempotent.

use std::path::{Path, PathBuf};

use domyjob_core::{jsonc, mcp_clients};
use serde::{Deserialize, Serialize};

use super::ops::{self, OpsError};
use super::store::{LinkState, Store, StoreError};
use super::sync::{self, Ssh, SyncError};
use crate::lock::{OsLock, Probe};
use crate::platform::clock::Deadline;
use crate::platform::service::{self as manager, ServiceError};
use crate::platform::user_files::{self, UserFileError};
use crate::process::{Tool, ToolError, ToolOutput, run_tool};
use crate::state_io::{self, StateError};

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
}

/// What setup and doctor report, one line per step or finding.
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

/// What the installed service runs, recorded when it was installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installed {
    program: String,
}

fn installed_path(store: &Store) -> PathBuf {
    store.paths().service_record()
}

fn installed(store: &Store) -> Result<Option<Installed>, SetupError> {
    match state_io::read_bytes(&installed_path(store))? {
        Some(bytes) => Ok(Some(domyjob_core::ingress::json(
            &bytes,
            domyjob_core::wire::MAX_CONTROL_BYTES,
        )?)),
        None => Ok(None),
    }
}

/// This build's executable at its stable, build-specific path.
fn stable_program() -> Result<PathBuf, SetupError> {
    let target = crate::platform::stable_program(&crate::identity::tag())?;
    user_files::install_executable(&std::env::current_exe()?, &target)?;
    Ok(target)
}

fn utf8(path: &Path) -> Result<&str, SetupError> {
    path.to_str().ok_or(SetupError::Path)
}

pub(crate) fn service_install(store: &Store) -> Result<Finding, SetupError> {
    let program = stable_program()?;
    let place = manager::install(&program, &store.paths().service_log())?;
    let record = Installed {
        program: utf8(&program)?.to_owned(),
    };
    state_io::write_bytes(&installed_path(store), &serde_json::to_vec(&record)?)?;
    Ok(Finding::ok(
        "service",
        format!("installed {} running {}", place.display(), record.program),
    ))
}

pub(crate) fn service_uninstall(store: &Store) -> Result<Finding, SetupError> {
    let removed = manager::uninstall()?;
    if OsLock::probe(&store.paths().service_lock())? == Probe::Held
        && let Some(bytes) = state_io::read_bytes(&store.paths().service_pid())?
        && let Ok(Ok(pid)) = std::str::from_utf8(&bytes).map(|text| text.trim().parse::<u32>())
    {
        crate::process::terminate(pid)?;
    }
    state_io::remove_file(&installed_path(store))?;
    Ok(Finding::ok(
        "service",
        if removed {
            "uninstalled"
        } else {
            "was not installed"
        },
    ))
}

/// Whether the service is installed, current, and running.
pub(crate) fn service_status(store: &Store) -> Result<Finding, SetupError> {
    let running = OsLock::probe(&store.paths().service_lock())? == Probe::Held;
    let current = crate::platform::stable_program(&crate::identity::tag())?;
    Ok(match (installed(store)?, running) {
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

fn tool(program: &str, arguments: &[String]) -> Result<Option<ToolOutput>, SetupError> {
    match run_tool(&Tool::new(program, arguments)) {
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

/// Whether each installed client runs `program mcp` as its `domyjob` server; `repair` fixes it.
fn clients(program: &str, repair: bool) -> Result<Vec<Finding>, SetupError> {
    let mut findings = Vec::new();
    let fix = "domyjob chat setup";
    if let Some(output) = tool(mcp_clients::CLAUDE, &mcp_clients::claude_get())? {
        let registered =
            output.success && mcp_clients::claude_registration_matches(&output.stdout, program);
        if !registered && repair {
            tool(mcp_clients::CLAUDE, &mcp_clients::claude_remove())?;
            tool(mcp_clients::CLAUDE, &mcp_clients::claude_add(program))?;
        }
        findings.push(if registered || repair {
            Finding::ok("claude", "MCP server registered for this user")
        } else {
            Finding::problem(
                "claude",
                "the domyjob MCP server is missing or points elsewhere",
                fix,
            )
        });
    }
    if let Some(output) = tool(mcp_clients::CODEX, &mcp_clients::codex_get())? {
        let registered =
            output.success && mcp_clients::codex_registration_matches(&output.stdout, program);
        if !registered && repair {
            tool(mcp_clients::CODEX, &mcp_clients::codex_remove())?;
            tool(mcp_clients::CODEX, &mcp_clients::codex_add(program))?;
        }
        findings.push(if registered || repair {
            Finding::ok("codex", "MCP server registered")
        } else {
            Finding::problem(
                "codex",
                "the domyjob MCP server is missing or points elsewhere",
                fix,
            )
        });
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

/// What `chat setup` does besides pinning its machines.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Steps {
    pub(crate) replace: bool,
    pub(crate) service: bool,
    pub(crate) clients: bool,
}

/// Pin peers, publish this machine, start the service, and register the AI clients.
pub(crate) fn setup(machines: &[String], steps: Steps) -> Result<Vec<Finding>, SetupError> {
    let Steps {
        replace,
        service,
        clients: register,
    } = steps;
    let store = Store::open()?;
    let mut findings = Vec::new();
    for alias in machines {
        let origin = sync::identify(&mut Ssh::new(alias)?, Deadline::after_seconds(900))?;
        store.pin(alias, &origin, replace)?;
        findings.push(Finding::ok(
            "peer",
            format!("{alias} is chat machine {origin}"),
        ));
    }
    ops::publish_machine(&store)?;
    if service {
        findings.push(service_install(&store)?);
    }
    if register {
        let program = stable_program()?;
        findings.extend(clients(utf8(&program)?, true)?);
    }
    let report = sync::sync(&store, None, Deadline::after_seconds(120))?;
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
    let Some(output) = tool(tool_name, &arguments)? else {
        return Ok(None);
    };
    let signed_in = if credentials {
        !output.stdout.contains("0 credentials")
    } else {
        output.success
    };
    Ok(Some(if signed_in {
        Finding::ok(tool_name, "signed in")
    } else {
        Finding::problem(
            tool_name,
            "not signed in",
            format!("sign in with the {tool_name} CLI"),
        )
    }))
}

/// Check the service, the links, the client registrations and logins, and the local agents.
pub(crate) fn doctor() -> Result<Vec<Finding>, SetupError> {
    let store = Store::open()?;
    let mut findings = vec![service_status(&store)?];
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
    findings.extend(agents(&store)?);
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
