use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::{Host, MachineName, RefPattern};
use crate::template::{Argv, TemplateError, Text};

const BUILTIN: &str = include_str!("builtin.toml");

pub const TRANSPORT_VARS: &[&str] = &[
    "host",
    "name",
    "self",
    "remote",
    "remote_argv",
    "cache",
    "home",
    "session",
];
pub const RESOLVE_VARS: &[&str] = &["root", "rev"];
pub const LIST_VARS: &[&str] = &["root", "commit"];
pub const SHOW_VARS: &[&str] = &["root", "commit", "path"];
pub const RUNNER_VARS: &[&str] = &["input", "words"];
pub const URL_VARS: &[&str] = &["version", "target", "exe"];
pub const FETCH_VARS: &[&str] = &["url", "output"];
pub const UNPACK_VARS: &[&str] = &["archive", "dir"];
pub const NOTIFIER_VARS: &[&str] = &["target"];
pub const SERVICE_VARS: &[&str] = &[
    "home",
    "state",
    "exe",
    "exe_xml",
    "arguments",
    "plist_arguments",
    "log",
    "log_xml",
    "uid",
    "action",
];
pub const MAX_MACHINES: usize = 64;
pub const MAX_SELECTOR_TERMS: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error("{origin}: {source}")]
    Parse {
        origin: String,
        source: Box<toml::de::Error>,
    },
    #[error("{origin} exceeds its {limit}-byte config budget")]
    TooLarge { origin: String, limit: u64 },
    #[error("{kind} {name}: {source}")]
    Template {
        kind: &'static str,
        name: String,
        source: TemplateError,
    },
    #[error("machine {machine} uses transport {transport}, which is not defined")]
    UnknownTransport {
        machine: MachineName,
        transport: String,
    },
    #[error("{selector} matches no machine")]
    NoMatch { selector: String },
    #[error("no group @{0}")]
    UnknownGroup(String),
    #[error("groups nest more than 8 deep at @{0}")]
    TooDeep(String),
    #[error("no machine has label {0}")]
    NoLabel(String),
    #[error(
        "{0:?} is neither a machine, a group, nor a label; an ssh host not yet added is written ssh:HOST"
    )]
    BadTerm(String),
    #[error("a worker checking machine {0}'s labels panicked")]
    FactWorkerPanicked(MachineName),
    #[error("no {kind} named {name}")]
    Missing { kind: &'static str, name: String },
    #[error("{path} is not valid TOML: {source}")]
    Edit {
        path: PathBuf,
        source: Box<toml_edit::TomlError>,
    },
    #[error(transparent)]
    Write(crate::failure::IoFailure),
    #[error("machine {0} is already configured")]
    Exists(MachineName),
    #[error("machine {0} is not configured")]
    NotConfigured(MachineName),
    #[error("{name} is not a configured machine{}", nearest.as_ref().map_or_else(String::new, |nearest| format!("; did you mean {nearest}?")))]
    Unknown {
        name: MachineName,
        nearest: Option<MachineName>,
    },
    #[error("{0} is this machine")]
    ThisMachine(MachineName),
    #[error("project job policy root {0} must be an absolute path")]
    ProjectRoot(PathBuf),
    #[error("project job policy for {0} has no permitted machines")]
    EmptyProjectMachines(PathBuf),
    #[error("project job policy for {root} permits {count} machines, above the limit of {limit}")]
    TooManyProjectMachines {
        root: PathBuf,
        count: usize,
        limit: usize,
    },
    #[error("project job policy for {root} names unconfigured machine {machine}")]
    ProjectMachineUnknown { root: PathBuf, machine: MachineName },
    #[error("project job policy repeats root {0}")]
    DuplicateProjectRoot(PathBuf),
    #[error("no local project job policy permits {0}; add a [[project_jobs]] entry to config.toml")]
    ProjectNotApproved(PathBuf),
    #[error("project job target {machine} is not permitted for {root}")]
    ProjectMachineDenied { root: PathBuf, machine: MachineName },
    #[error("project job target {0:?} must be a configured machine name or @all")]
    ProjectSelector(String),
    #[error("{count} machines are configured, above the limit of {limit}")]
    TooManyMachines { count: usize, limit: usize },
    #[error("machine selection exceeds the limit of {limit} targets")]
    TooManyTargets { limit: usize },
    #[error("machine selection expands more than {limit} terms")]
    SelectorTooComplex { limit: usize },
}

struct SelectorBudget {
    remaining: usize,
}

struct SelectorContext<'a> {
    depth: u8,
    budget: &'a mut SelectorBudget,
}

impl SelectorBudget {
    const fn new() -> Self {
        Self {
            remaining: MAX_SELECTOR_TERMS,
        }
    }

    fn consume(&mut self) -> Result<(), ConfigError> {
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or(ConfigError::SelectorTooComplex {
                limit: MAX_SELECTOR_TERMS,
            })?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote(Machine);

impl Remote {
    #[must_use]
    pub const fn machine(&self) -> &Machine {
        &self.0
    }
}

pub const LOCAL: &str = "local";
pub const AD_HOC: &str = "ssh:";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    defaults: Option<Defaults>,
    machines: Option<BTreeMap<MachineName, MachineConf>>,
    groups: Option<BTreeMap<String, Vec<String>>>,
    transports: Option<BTreeMap<String, TransportConf>>,
    sources: Option<BTreeMap<String, SourceConf>>,
    runners: Option<BTreeMap<String, RunnerConf>>,
    notifiers: Option<BTreeMap<String, NotifierConf>>,
    distribution: Option<Distribution>,
    triggers: Option<BTreeMap<String, TriggerConf>>,
    mcp: Option<McpPolicy>,
    services: Option<BTreeMap<String, ServiceConf>>,
    project_jobs: Option<Vec<LocalPolicy>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalPolicy {
    root: PathBuf,
    machines: BTreeSet<MachineName>,
}

impl LocalPolicy {
    #[must_use]
    pub(crate) const fn machines(&self) -> &BTreeSet<MachineName> {
        &self.machines
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTool {
    Machines,
    ListJobs,
    JobStatus,
    JobLogs,
    JobDigest,
    SearchLogs,
    WaitJob,
    GetFile,
    Run,
    KillJob,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpPolicy {
    pub tools: BTreeSet<McpTool>,
    pub machines: String,
    pub runners: Vec<String>,
    pub directories: Vec<PathBuf>,
}

impl McpPolicy {
    #[must_use]
    pub fn read_only() -> Self {
        Self {
            tools: [
                McpTool::Machines,
                McpTool::ListJobs,
                McpTool::JobStatus,
                McpTool::JobLogs,
                McpTool::JobDigest,
                McpTool::SearchLogs,
                McpTool::WaitJob,
            ]
            .into_iter()
            .collect(),
            machines: String::new(),
            runners: Vec::new(),
            directories: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerConf {
    pub source: String,
    pub repository: PathBuf,
    pub event: String,
    pub refs: Vec<RefPattern>,
    pub on: String,
    pub run: Vec<ConfigText>,
    pub runner: Option<String>,
    pub notify: Option<Vec<ConfigText>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    pub manifest: Text,
    pub latest: Text,
    pub archive: Text,
    pub fetch: Argv,
    pub unpack: Argv,
    pub binary: Text,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub defaults: Defaults,
    pub machines: BTreeMap<MachineName, MachineConf>,
    pub groups: BTreeMap<String, Vec<String>>,
    pub transports: BTreeMap<String, TransportConf>,
    pub sources: BTreeMap<String, SourceConf>,
    pub runners: BTreeMap<String, RunnerConf>,
    pub notifiers: BTreeMap<String, NotifierConf>,
    pub distribution: Option<Distribution>,
    pub triggers: BTreeMap<String, TriggerConf>,
    pub mcp: McpPolicy,
    pub services: BTreeMap<String, ServiceConf>,
    project_jobs: Vec<LocalPolicy>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConf {
    pub file: Option<Text>,
    pub contents: Option<Text>,
    pub probe: Option<Argv>,
    pub install: Vec<ServiceAction>,
    pub uninstall: Vec<ServiceAction>,
    pub installed: Text,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceAction {
    pub run: Argv,
    pub tolerate_failure: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ConfigText(String);

impl ConfigText {
    #[must_use]
    pub fn as_config_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn split_once(&self, separator: char) -> (Self, Option<Self>) {
        match self.0.split_once(separator) {
            Some((head, tail)) => (Self(head.to_owned()), Some(Self(tail.to_owned()))),
            None => (self.clone(), None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StdinFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub transport: Option<String>,
    pub notify: Option<Vec<ConfigText>>,
}

impl Defaults {
    const fn empty() -> Self {
        Self {
            transport: None,
            notify: None,
        }
    }
}

impl File {
    fn into_config(self) -> Config {
        Config {
            defaults: self.defaults.unwrap_or_else(Defaults::empty),
            machines: self.machines.unwrap_or_default(),
            groups: self.groups.unwrap_or_default(),
            transports: self.transports.unwrap_or_default(),
            sources: self.sources.unwrap_or_default(),
            runners: self.runners.unwrap_or_default(),
            notifiers: self.notifiers.unwrap_or_default(),
            distribution: self.distribution,
            triggers: self.triggers.unwrap_or_default(),
            mcp: self.mcp.unwrap_or_else(McpPolicy::read_only),
            services: self.services.unwrap_or_default(),
            project_jobs: self.project_jobs.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConf {
    pub host: Option<Host>,
    pub transport: Option<String>,
    pub labels: Option<Vec<String>>,
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Binary {
    Upload,
    Present,
    #[serde(rename = "self")]
    Itself,
}

#[derive(Debug, Clone, Copy)]
pub struct PerOs<'a> {
    run: &'a Argv,
    macos: Option<&'a Argv>,
    linux: Option<&'a Argv>,
    windows: Option<&'a Argv>,
}

impl<'a> PerOs<'a> {
    #[must_use]
    pub fn for_os(self, os: &str) -> &'a Argv {
        let specific = match os {
            "macos" => self.macos,
            "linux" => self.linux,
            "windows" => self.windows,
            _ => None,
        };
        specific.unwrap_or(self.run)
    }

    fn all(self) -> impl Iterator<Item = &'a Argv> {
        std::iter::once(self.run)
            .chain(self.macos)
            .chain(self.linux)
            .chain(self.windows)
    }

    #[must_use]
    pub fn client(self) -> &'a Argv {
        self.for_os(crate::platform::OS)
    }
}

macro_rules! per_os {
    ($type:ty) => {
        impl $type {
            #[must_use]
            pub const fn command(&self) -> PerOs<'_> {
                PerOs {
                    run: &self.run,
                    macos: self.run_macos.as_ref(),
                    linux: self.run_linux.as_ref(),
                    windows: self.run_windows.as_ref(),
                }
            }
        }
    };
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConf {
    pub binary: Binary,
    run: Argv,
    run_macos: Option<Argv>,
    run_linux: Option<Argv>,
    run_windows: Option<Argv>,
    share: Option<Argv>,
    share_windows: Option<Argv>,
}

per_os!(TransportConf);

impl TransportConf {
    #[must_use]
    pub const fn sharing(&self) -> Option<&Argv> {
        match crate::platform::FAMILY {
            crate::paths::Family::Windows => self.share_windows.as_ref(),
            crate::paths::Family::Unix => self.share.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Separator {
    Nul,
    Newline,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConf {
    pub detect: String,
    pub priority: i32,
    pub separator: Separator,
    pub resolve: Argv,
    pub list: Argv,
    pub show: Argv,
    pub identity: Option<Argv>,
    pub unset: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum RunnerConf {
    Script { run: Text },
    Argv { run: Argv },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifierConf {
    pub stdin: StdinFormat,
    run: Argv,
    run_macos: Option<Argv>,
    run_linux: Option<Argv>,
    run_windows: Option<Argv>,
}

per_os!(NotifierConf);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub name: MachineName,
    pub host: Host,
    pub transport: String,
    pub labels: Vec<String>,
    pub shell: Option<String>,
}

#[must_use]
pub fn path(dirs: &crate::paths::Dirs) -> PathBuf {
    dirs.config.join("config.toml")
}

fn parse(text: &str, origin: &str) -> Result<Config, ConfigError> {
    if crate::domain::len_u64(text.len()) > crate::bounded::CONFIG_TEXT {
        return Err(ConfigError::TooLarge {
            origin: origin.to_owned(),
            limit: crate::bounded::CONFIG_TEXT,
        });
    }
    crate::ingress::toml::<File>(text)
        .map(File::into_config)
        .map_err(|source| ConfigError::Parse {
            origin: origin.to_owned(),
            source: Box::new(source),
        })
}

impl Config {
    pub fn builtin() -> Result<Self, ConfigError> {
        let config = parse(BUILTIN, "built-in definitions")?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match crate::bounded::text_file(path, crate::bounded::CONFIG_TEXT) {
            Ok(text) => Self::layered(&text, &path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::builtin(),
            Err(source) => Err(crate::failure::io("reading", path)(source).into()),
        }
    }

    pub fn layered(text: &str, origin: &str) -> Result<Self, ConfigError> {
        let mut merged = parse(BUILTIN, "built-in definitions")?;
        let user = parse(text, origin)?;
        merged.defaults = user.defaults;
        merged.machines = user.machines;
        merged.groups = user.groups;
        merged.transports.extend(user.transports);
        merged.sources.extend(user.sources);
        merged.runners.extend(user.runners);
        merged.notifiers.extend(user.notifiers);
        if user.distribution.is_some() {
            merged.distribution = user.distribution;
        }
        merged.triggers = user.triggers;
        merged.mcp = user.mcp;
        merged.project_jobs = user.project_jobs;

        merged.validate()?;
        Ok(merged)
    }

    fn validate_service(name: &str, service: &ServiceConf) -> Result<(), ConfigError> {
        let invalid = |source| ConfigError::Template {
            kind: "service",
            name: name.to_owned(),
            source,
        };
        for text in service
            .file
            .iter()
            .chain(&service.contents)
            .chain(std::iter::once(&service.installed))
        {
            text.check(SERVICE_VARS).map_err(&invalid)?;
        }
        for argv in service
            .probe
            .iter()
            .chain(service.install.iter().map(|action| &action.run))
            .chain(service.uninstall.iter().map(|action| &action.run))
        {
            argv.check(SERVICE_VARS).map_err(&invalid)?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.machines.len() > MAX_MACHINES {
            return Err(ConfigError::TooManyMachines {
                count: self.machines.len(),
                limit: MAX_MACHINES,
            });
        }
        let template = |kind: &'static str, name: &str| {
            let name = name.to_owned();
            move |source| ConfigError::Template { kind, name, source }
        };
        for (name, transport) in &self.transports {
            let shares = transport.share.iter().chain(&transport.share_windows);
            for argv in transport.command().all().chain(shares) {
                argv.check(TRANSPORT_VARS)
                    .map_err(template("transport", name))?;
            }
        }
        for (name, source) in &self.sources {
            source
                .resolve
                .check(RESOLVE_VARS)
                .map_err(template("source", name))?;
            source
                .list
                .check(LIST_VARS)
                .map_err(template("source", name))?;
            source
                .show
                .check(SHOW_VARS)
                .map_err(template("source", name))?;
        }
        for (name, runner) in &self.runners {
            match runner {
                RunnerConf::Script { run } => run.check(RUNNER_VARS),
                RunnerConf::Argv { run } => run.check(RUNNER_VARS),
            }
            .map_err(template("runner", name))?;
        }
        for (name, notifier) in &self.notifiers {
            for argv in notifier.command().all() {
                argv.check(NOTIFIER_VARS)
                    .map_err(template("notifier", name))?;
            }
        }
        for (name, service) in &self.services {
            Self::validate_service(name, service)?;
        }
        if let Some(distribution) = &self.distribution {
            for (field, text) in [
                ("manifest", &distribution.manifest),
                ("latest", &distribution.latest),
                ("archive", &distribution.archive),
                ("binary", &distribution.binary),
            ] {
                text.check(URL_VARS)
                    .map_err(template("distribution", field))?;
            }
            distribution
                .fetch
                .check(FETCH_VARS)
                .map_err(template("distribution", "fetch"))?;
            distribution
                .unpack
                .check(UNPACK_VARS)
                .map_err(template("distribution", "unpack"))?;
        }
        for name in self.machines.keys() {
            let machine = self.build(name);
            if !self.transports.contains_key(&machine.transport) {
                return Err(ConfigError::UnknownTransport {
                    machine: machine.name,
                    transport: machine.transport,
                });
            }
        }
        self.validate_project_policies()
    }

    fn validate_project_policies(&self) -> Result<(), ConfigError> {
        let mut roots = BTreeSet::new();
        for policy in &self.project_jobs {
            if !policy.root.is_absolute() {
                return Err(ConfigError::ProjectRoot(policy.root.clone()));
            }
            if !roots.insert(&policy.root) {
                return Err(ConfigError::DuplicateProjectRoot(policy.root.clone()));
            }
            if policy.machines.is_empty() {
                return Err(ConfigError::EmptyProjectMachines(policy.root.clone()));
            }
            if policy.machines.len() > MAX_MACHINES {
                return Err(ConfigError::TooManyProjectMachines {
                    root: policy.root.clone(),
                    count: policy.machines.len(),
                    limit: MAX_MACHINES,
                });
            }
            for machine in &policy.machines {
                if machine.as_str() != LOCAL && !self.machines.contains_key(machine) {
                    return Err(ConfigError::ProjectMachineUnknown {
                        root: policy.root.clone(),
                        machine: machine.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub(crate) fn project_policy(
        &self,
        root: &Path,
    ) -> Result<(PathBuf, &LocalPolicy), ConfigError> {
        let resolved =
            std::fs::canonicalize(root).map_err(crate::failure::io("resolving", root))?;
        let mut approved = None;
        for policy in &self.project_jobs {
            if std::fs::canonicalize(&policy.root).is_ok_and(|known| known == resolved) {
                if approved.is_some() {
                    return Err(ConfigError::DuplicateProjectRoot(resolved));
                }
                approved = Some(policy);
            }
        }
        approved
            .map(|policy| (resolved.clone(), policy))
            .ok_or(ConfigError::ProjectNotApproved(resolved))
    }

    fn default_transport(&self) -> String {
        self.defaults
            .transport
            .clone()
            .unwrap_or_else(|| "ssh".to_owned())
    }

    pub fn machine(&self, name: &MachineName) -> Result<Machine, ConfigError> {
        if self.machines.contains_key(name) || name.as_str() == LOCAL {
            Ok(self.build(name))
        } else {
            Err(ConfigError::Unknown {
                name: name.clone(),
                nearest: self.nearest(name),
            })
        }
    }

    pub fn remote(&self, name: &MachineName) -> Result<Remote, ConfigError> {
        if !self.machines.contains_key(name) {
            return Err(ConfigError::NotConfigured(name.clone()));
        }
        let machine = self.build(name);
        if machine.transport == LOCAL {
            return Err(ConfigError::ThisMachine(name.clone()));
        }
        Ok(Remote(machine))
    }

    fn nearest(&self, name: &MachineName) -> Option<MachineName> {
        self.machines
            .keys()
            .map(|known| (strsim::jaro_winkler(known.as_str(), name.as_str()), known))
            .filter(|(likeness, _)| *likeness >= 0.8)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, known)| known.clone())
    }

    fn build(&self, name: &MachineName) -> Machine {
        let conf = self.machines.get(name);
        let local = name.as_str() == LOCAL;
        let transport = conf.and_then(|c| c.transport.clone()).unwrap_or_else(|| {
            if local {
                LOCAL.to_owned()
            } else {
                self.default_transport()
            }
        });
        Machine {
            name: name.clone(),
            host: conf
                .and_then(|c| c.host.clone())
                .unwrap_or_else(|| name.to_host()),
            transport,
            labels: conf.and_then(|c| c.labels.clone()).unwrap_or_default(),
            shell: conf.and_then(|c| c.shell.clone()),
        }
    }

    #[must_use]
    pub fn configured(&self) -> Vec<Machine> {
        self.machines.keys().map(|name| self.build(name)).collect()
    }

    pub fn transport(&self, name: &str) -> Result<&TransportConf, ConfigError> {
        self.transports
            .get(name)
            .ok_or_else(|| ConfigError::Missing {
                kind: "transport",
                name: name.to_owned(),
            })
    }

    pub fn runner(&self, name: &str) -> Result<&RunnerConf, ConfigError> {
        self.runners.get(name).ok_or_else(|| ConfigError::Missing {
            kind: "runner",
            name: name.to_owned(),
        })
    }

    pub fn notifier(&self, name: &str) -> Result<&NotifierConf, ConfigError> {
        self.notifiers
            .get(name)
            .ok_or_else(|| ConfigError::Missing {
                kind: "notifier",
                name: name.to_owned(),
            })
    }

    #[must_use]
    pub fn sources_by_priority(&self) -> Vec<(&str, &SourceConf)> {
        let mut sources: Vec<(&str, &SourceConf)> =
            self.sources.iter().map(|(n, s)| (n.as_str(), s)).collect();
        sources.sort_by_key(|(name, source)| (std::cmp::Reverse(source.priority), *name));
        sources
    }

    pub fn select<E>(
        &self,
        selector: &str,
        facts: &(dyn Fn(&Machine) -> Result<Vec<String>, E> + Sync),
    ) -> Result<Vec<Machine>, E>
    where
        E: From<ConfigError> + Send,
    {
        let mut budget = SelectorBudget::new();
        let mut context = SelectorContext {
            depth: 0,
            budget: &mut budget,
        };
        let out = self.select_at(selector, facts, &mut context)?;
        if out.is_empty() {
            return Err(E::from(ConfigError::NoMatch {
                selector: selector.to_owned(),
            }));
        }
        Ok(out)
    }

    fn select_at<E>(
        &self,
        selector: &str,
        facts: &(dyn Fn(&Machine) -> Result<Vec<String>, E> + Sync),
        context: &mut SelectorContext<'_>,
    ) -> Result<Vec<Machine>, E>
    where
        E: From<ConfigError> + Send,
    {
        let mut out: Vec<Machine> = Vec::new();
        for term in selector.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            context.budget.consume().map_err(E::from)?;
            for machine in self.term(term, facts, context)? {
                if !out.iter().any(|known| known.name == machine.name) {
                    if out.len() == MAX_MACHINES {
                        return Err(E::from(ConfigError::TooManyTargets {
                            limit: MAX_MACHINES,
                        }));
                    }
                    out.push(machine);
                }
            }
        }
        Ok(out)
    }

    fn term<E>(
        &self,
        term: &str,
        facts: &(dyn Fn(&Machine) -> Result<Vec<String>, E> + Sync),
        context: &mut SelectorContext<'_>,
    ) -> Result<Vec<Machine>, E>
    where
        E: From<ConfigError> + Send,
    {
        if let Some(group) = term.strip_prefix('@') {
            if group == "all" {
                return Ok(self.configured());
            }
            let members = self
                .groups
                .get(group)
                .ok_or_else(|| E::from(ConfigError::UnknownGroup(group.to_owned())))?;
            let deeper = context
                .depth
                .checked_add(1)
                .filter(|d| *d <= 8)
                .ok_or_else(|| E::from(ConfigError::TooDeep(group.to_owned())))?;
            return self.select_at(
                &members.join(","),
                facts,
                &mut SelectorContext {
                    depth: deeper,
                    budget: &mut *context.budget,
                },
            );
        }
        if let Some(host) = term.strip_prefix(AD_HOC) {
            return match host.parse::<MachineName>() {
                Ok(name) if !self.machines.contains_key(&name) => Ok(vec![self.build(&name)]),
                Ok(_) | Err(_) => Err(E::from(ConfigError::BadTerm(term.to_owned()))),
            };
        }
        if let Ok(name) = term.parse::<MachineName>()
            && self.machines.contains_key(&name)
        {
            return Ok(vec![self.build(&name)]);
        }
        let wanted: Vec<&str> = term.split('+').collect();
        let needs_facts = wanted.iter().any(|w| w.contains('='));
        let configured = self.configured();
        let learned = if needs_facts {
            learn(&configured, facts)?
        } else {
            vec![Vec::new(); configured.len()]
        };
        let matching: Vec<Machine> = configured
            .into_iter()
            .zip(learned)
            .filter(|(machine, learned)| {
                wanted
                    .iter()
                    .all(|w| machine.labels.iter().chain(learned).any(|label| label == w))
            })
            .map(|(machine, _)| machine)
            .collect();
        if !matching.is_empty() {
            return Ok(matching);
        }
        if needs_facts {
            return Err(E::from(ConfigError::NoLabel(term.to_owned())));
        }
        match term.parse::<MachineName>() {
            Ok(name) => Ok(vec![self.machine(&name)?]),
            Err(_invalid) => Err(E::from(ConfigError::BadTerm(term.to_owned()))),
        }
    }
}

fn learn<E>(
    machines: &[Machine],
    facts: &(dyn Fn(&Machine) -> Result<Vec<String>, E> + Sync),
) -> Result<Vec<Vec<String>>, E>
where
    E: From<ConfigError> + Send,
{
    std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every machine is asked before any answer is awaited"
        )]
        let handles: Vec<_> = machines
            .iter()
            .map(|machine| scope.spawn(move || facts(machine)))
            .collect();
        handles
            .into_iter()
            .zip(machines)
            .map(|(handle, machine)| match handle.join() {
                Ok(learned) => learned,
                Err(_panicked) => Err(E::from(ConfigError::FactWorkerPanicked(
                    machine.name.clone(),
                ))),
            })
            .collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMachine {
    pub name: MachineName,
    pub host: Option<Host>,
    pub transport: Option<String>,
    pub labels: Vec<String>,
}

fn edit(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<(), ConfigError>,
) -> Result<(), ConfigError> {
    let text = match crate::bounded::text_file(path, crate::bounded::CONFIG_TEXT) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(crate::failure::io("reading", path)(source).into()),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|source| ConfigError::Edit {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    change(&mut doc)?;
    let updated = doc.to_string();
    Config::layered(&updated, &path.display().to_string())?;
    crate::user_files::write(path, updated.as_bytes()).map_err(ConfigError::Write)
}

pub fn add_machine(path: &Path, machine: &NewMachine) -> Result<(), ConfigError> {
    edit(path, |doc| {
        let machines = doc.entry("machines").or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        });
        let Some(machines) = machines.as_table_mut() else {
            return Err(ConfigError::Exists(machine.name.clone()));
        };
        if machines.contains_key(machine.name.as_str()) {
            return Err(ConfigError::Exists(machine.name.clone()));
        }
        let mut entry = toml_edit::Table::new();
        if let Some(host) = &machine.host {
            entry.insert("host", toml_edit::value(host.as_str()));
        }
        if let Some(transport) = &machine.transport {
            entry.insert("transport", toml_edit::value(transport.as_str()));
        }
        if !machine.labels.is_empty() {
            let labels: toml_edit::Array = machine.labels.iter().map(String::as_str).collect();
            entry.insert("labels", toml_edit::value(labels));
        }
        machines.insert(machine.name.as_str(), toml_edit::Item::Table(entry));
        Ok(())
    })
}

pub fn remove_machine(path: &Path, name: &MachineName) -> Result<(), ConfigError> {
    edit(path, |doc| {
        let removed = doc
            .get_mut("machines")
            .and_then(toml_edit::Item::as_table_mut)
            .and_then(|machines| machines.remove(name.as_str()));
        match removed {
            Some(_) => Ok(()),
            None => Err(ConfigError::NotConfigured(name.clone())),
        }
    })
}

impl crate::ingress::Ingress for File {}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build their fixtures directly on disk"
)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[machines.box]
host = "me@build-box"
labels = ["gpu"]

[machines.win]
labels = ["fast"]

[machines.pod]
transport = "kube"
host = "builder-0"

[transports.kube]
binary = "upload"
run = ["kubectl", "exec", "-i", "{host}", "--", "{remote_argv...}"]

[runners.agent]
kind = "argv"
run = ["agent", "run", "{input}"]

[groups]
heavy = ["box", "win"]
everything = ["@heavy", "pod"]
"#;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the test callback shares the fallible selector signature"
    )]
    fn facts(machine: &Machine) -> Result<Vec<String>, ConfigError> {
        Ok(match machine.name.as_str() {
            "win" => vec!["os=windows".into()],
            _ => vec!["os=linux".into()],
        })
    }

    fn names(machines: Vec<Machine>) -> Vec<String> {
        machines
            .into_iter()
            .map(|m| m.name.as_str().to_owned())
            .collect()
    }

    #[test]
    fn builtins_are_ordinary_definitions() {
        let config = Config::builtin().unwrap();
        assert!(config.transports.contains_key("ssh"));
        assert_eq!(config.transport("local").unwrap().binary, Binary::Itself);
        let order: Vec<&str> = config
            .sources_by_priority()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(order, ["jj", "git"]);
    }

    #[test]
    fn local_project_policy_rejects_ambiguous_or_unconfigured_grants() {
        let relative = "[[project_jobs]]\nroot = 'relative'\nmachines = ['local']\n";
        assert!(matches!(
            Config::layered(relative, "relative"),
            Err(ConfigError::ProjectRoot(_))
        ));
        let root = tempfile::tempdir().unwrap();
        let path = root.path().display();
        let empty = format!("[[project_jobs]]\nroot = '{path}'\nmachines = []\n");
        assert!(matches!(
            Config::layered(&empty, "empty"),
            Err(ConfigError::EmptyProjectMachines(_))
        ));
        let unknown = format!("[[project_jobs]]\nroot = '{path}'\nmachines = ['elsewhere']\n");
        assert!(matches!(
            Config::layered(&unknown, "unknown"),
            Err(ConfigError::ProjectMachineUnknown { .. })
        ));
        let duplicate = format!(
            "[[project_jobs]]\nroot = '{path}'\nmachines = ['local']\n[[project_jobs]]\nroot = '{path}'\nmachines = ['local']\n"
        );
        assert!(matches!(
            Config::layered(&duplicate, "duplicate"),
            Err(ConfigError::DuplicateProjectRoot(_))
        ));
        crate::state_file::private_dir(&root.path().join("inside")).unwrap();
        let canonical_duplicate = format!(
            "[[project_jobs]]\nroot = '{path}'\nmachines = ['local']\n[[project_jobs]]\nroot = '{path}/inside/..'\nmachines = ['local']\n"
        );
        let config = Config::layered(&canonical_duplicate, "same directory").unwrap();
        assert!(matches!(
            config.project_policy(root.path()),
            Err(ConfigError::DuplicateProjectRoot(_))
        ));
    }

    #[test]
    fn machine_and_target_counts_have_a_fixed_budget() {
        use std::fmt::Write as _;

        let mut configured = String::new();
        for number in 0..MAX_MACHINES {
            writeln!(&mut configured, "[machines.m{number}]").unwrap();
        }
        let config = Config::layered(&configured, "machine limit").unwrap();
        assert_eq!(config.configured().len(), MAX_MACHINES);
        let too_many = format!("{configured}[machines.extra]\n");
        assert!(matches!(
            Config::layered(&too_many, "too many machines"),
            Err(ConfigError::TooManyMachines { .. })
        ));
        let root = tempfile::tempdir().unwrap();
        let policy_names = std::iter::once("'local'".to_owned())
            .chain((0..MAX_MACHINES).map(|number| format!("'m{number}'")))
            .collect::<Vec<_>>()
            .join(",");
        let oversized_policy = format!(
            "{configured}[[project_jobs]]\nroot = '{}'\nmachines = [{policy_names}]\n",
            root.path().display()
        );
        assert!(matches!(
            Config::layered(&oversized_policy, "too many project targets"),
            Err(ConfigError::TooManyProjectMachines { .. })
        ));

        let selector = (0..=MAX_MACHINES)
            .map(|number| format!("ssh:host{number}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(matches!(
            config.select(&selector, &facts),
            Err(ConfigError::TooManyTargets { .. })
        ));
        let repeated = std::iter::repeat_n("ssh:one", MAX_MACHINES + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(names(config.select(&repeated, &facts).unwrap()), ["one"]);

        let group_members = std::iter::repeat_n("'m0'", MAX_SELECTOR_TERMS / 2 + 1)
            .collect::<Vec<_>>()
            .join(",");
        let grouped = format!(
            "{configured}[groups]\nfirst = [{group_members}]\nsecond = [{group_members}]\n"
        );
        let grouped = Config::layered(&grouped, "wide groups").unwrap();
        assert!(matches!(
            grouped.select("@first,@second", &facts),
            Err(ConfigError::SelectorTooComplex { .. })
        ));
    }

    #[test]
    fn every_machine_is_asked_for_its_facts_at_once() {
        let config = Config::layered(SAMPLE, "sample").unwrap();
        let everyone = std::sync::Barrier::new(config.configured().len());
        let together = |machine: &Machine| {
            everyone.wait();
            facts(machine)
        };
        assert_eq!(
            names(config.select("os=linux", &together).unwrap()),
            ["box", "pod"]
        );
    }

    #[test]
    fn a_failed_fact_query_cannot_silently_drop_a_machine_from_selection() {
        let config = Config::layered(SAMPLE, "sample").unwrap();
        let fallible = |machine: &Machine| {
            if machine.name.as_str() == "win" {
                Err(ConfigError::Io(crate::failure::IoFailure {
                    action: "reading",
                    path: PathBuf::from("facts"),
                    source: std::io::Error::other("unreachable"),
                }))
            } else {
                facts(machine)
            }
        };
        assert!(matches!(
            config.select("os=linux", &fallible),
            Err(ConfigError::Io(_))
        ));
    }

    #[test]
    fn selectors_cover_names_groups_labels_and_hosts() {
        let config = Config::layered(SAMPLE, "sample").unwrap();
        assert_eq!(names(config.select("box", &facts).unwrap()), ["box"]);
        assert_eq!(
            names(config.select("@everything", &facts).unwrap()),
            ["box", "win", "pod"]
        );
        assert_eq!(
            names(config.select("os=linux", &facts).unwrap()),
            ["box", "pod"]
        );
        assert_eq!(
            names(config.select("os=linux+gpu", &facts).unwrap()),
            ["box"]
        );
        assert_eq!(
            names(config.select("@all", &facts).unwrap()),
            ["box", "pod", "win"]
        );
        assert!(matches!(
            config.select("os=plan9", &facts),
            Err(ConfigError::NoLabel(_))
        ));
        assert!(matches!(
            config.select("@nope", &facts),
            Err(ConfigError::UnknownGroup(_))
        ));
        assert!(matches!(
            config.select("-oProxy", &facts),
            Err(ConfigError::BadTerm(_))
        ));
        let named = |name: &str| config.machine(&name.parse().unwrap()).unwrap();
        assert_eq!(named("local").transport, "local");
    }

    #[test]
    fn a_name_nobody_configured_is_never_taken_for_a_host() {
        let config = Config::layered(SAMPLE, "sample").unwrap();
        assert!(matches!(
            config.select("wim", &facts),
            Err(ConfigError::Unknown { nearest: Some(near), .. }) if near.as_str() == "win"
        ));
        assert!(matches!(
            config.select("elsewhere", &facts),
            Err(ConfigError::Unknown { nearest: None, .. })
        ));
        assert!(matches!(
            config.select("user@elsewhere", &facts),
            Err(ConfigError::Unknown { .. })
        ));
        let direct = config.select("ssh:user@elsewhere", &facts).unwrap();
        assert_eq!(direct.first().unwrap().host.as_str(), "user@elsewhere");
        assert_eq!(direct.first().unwrap().transport, "ssh");
        assert!(matches!(
            config.select("ssh:win", &facts),
            Err(ConfigError::BadTerm(_))
        ));
        assert!(matches!(
            config.remote(&"local".parse().unwrap()),
            Err(ConfigError::NotConfigured(_))
        ));
        config.remote(&"win".parse().unwrap()).unwrap();
    }

    #[test]
    fn machines_are_added_without_disturbing_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[defaults]\ntransport = \"ssh\" # keep this note\n").unwrap();
        let machine = NewMachine {
            name: "box".parse().unwrap(),
            host: Some("me@box".parse().unwrap()),
            transport: None,
            labels: vec!["gpu".into()],
        };
        add_machine(&path, &machine).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this note"));
        assert!(text.contains("[machines.box]"));
        assert!(matches!(
            add_machine(&path, &machine),
            Err(ConfigError::Exists(_))
        ));
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config
                .machine(&"box".parse().unwrap())
                .unwrap()
                .host
                .as_str(),
            "me@box"
        );
        let broken = NewMachine {
            transport: Some("pigeon".into()),
            name: "b2".parse().unwrap(),
            ..machine.clone()
        };
        assert!(matches!(
            add_machine(&path, &broken),
            Err(ConfigError::UnknownTransport { .. })
        ));
        remove_machine(&path, &machine.name).unwrap();
        assert!(matches!(
            remove_machine(&path, &machine.name),
            Err(ConfigError::NotConfigured(_))
        ));
    }

    #[test]
    fn templates_are_checked_when_loaded() {
        let bad = "[runners.x]\nkind = \"argv\"\nrun = [\"{prompt}\"]\n";
        assert!(matches!(
            Config::layered(bad, "bad"),
            Err(ConfigError::Template { .. })
        ));
        let dangling = "[machines.a]\ntransport = \"carrier-pigeon\"\n";
        assert!(matches!(
            Config::layered(dangling, "bad"),
            Err(ConfigError::UnknownTransport { .. })
        ));
        assert!(matches!(
            Config::layered("[typo]\n", "bad"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn trigger_patterns_are_validated_while_loading_config() {
        let trigger = |pattern: &str| {
            format!(
                "[triggers.push]\nsource = \"git\"\nrepository = \".\"\nevent = \"push\"\nrefs = [\"{pattern}\"]\non = \"@all\"\nrun = [\"cargo test\"]\n"
            )
        };
        let config = Config::layered(&trigger("refs/heads/*"), "valid").unwrap();
        let pattern = config.triggers.get("push").unwrap().refs.first().unwrap();
        assert!(pattern.matches(&"refs/heads/main".parse().unwrap()));
        assert!(matches!(
            Config::layered(&trigger(&"*".repeat(1025)), "oversized"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn oversized_config_is_rejected_before_parsing_or_editing() {
        let huge = " ".repeat(4_194_305);
        assert!(matches!(
            Config::layered(&huge, "oversized"),
            Err(ConfigError::TooLarge { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, huge).unwrap();
        assert!(matches!(Config::load(&path), Err(ConfigError::Io(_))));
    }
}
