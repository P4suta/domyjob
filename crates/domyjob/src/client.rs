use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, ConfigError, Machine, RunnerConf};
use crate::domain::{
    BlobId, EnvName, Invalid, JobId, JobName, JobRef, MachineName, ProjectKey, RelPath,
};
use crate::paths::Dirs;
use crate::project::{self, ProjectError};
use crate::protocol::{
    Command, Follow, Job, Location, Reply, Request, Source, Submission, Workspace,
};
use crate::remote::{Link, RemoteError, cached_facts};
use crate::snapshot::{self, Entry, Origin, Snapshot, SnapshotError};
use crate::template::{Arg, Bindings, TemplateError};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error("runner {runner}: {source}")]
    Runner {
        runner: String,
        source: TemplateError,
    },
    #[error("nothing to run: give a command after --")]
    NoInput,
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} holds a malformed record: {source}")]
    Index {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "nothing called {0}: no job id, name, or `latest` that this machine started matches it"
    )]
    Unknown(String),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error("a worker thread panicked")]
    Panicked,
    #[error("{reference} could be any of {candidates}")]
    Ambiguous {
        reference: String,
        candidates: String,
    },
    #[error("{machine}: the changes it sent back are malformed: {why}")]
    Unpacked {
        machine: MachineName,
        why: Unpacking,
    },
    #[error("this machine has no note of a directory sent with {job}")]
    NotSent { job: JobId },
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> ClientError + use<> {
    let path = path.to_path_buf();
    move |source| ClientError::Io {
        action,
        path,
        source,
    }
}

#[derive(Debug, Clone)]
pub struct Context {
    pub config: Config,
    pub dirs: Dirs,
}

impl Context {
    pub fn load() -> Result<Self, ClientError> {
        let dirs = Dirs::from_env();
        let config = Config::load(&crate::config::path(&dirs))?;
        Ok(Self { config, dirs })
    }

    fn facts(&self, machine: &Machine) -> Vec<String> {
        match cached_facts(&self.dirs, machine) {
            Ok(Some(facts)) => facts.labels(),
            Ok(None) | Err(_) => match Link::open(&self.config, &self.dirs, machine) {
                Ok(_) => match cached_facts(&self.dirs, machine) {
                    Ok(Some(facts)) => facts.labels(),
                    Ok(None) | Err(_) => Vec::new(),
                },
                Err(error) => {
                    eprintln!("domyjob: {}: {error}", machine.name);
                    Vec::new()
                }
            },
        }
    }

    pub fn select(&self, selector: &str) -> Result<Vec<Machine>, ClientError> {
        Ok(self
            .config
            .select(selector, &|machine| self.facts(machine))?)
    }

    fn index_path(&self) -> PathBuf {
        self.dirs.state.join("client").join("index.jsonl")
    }

    fn origin(&self) -> Result<String, ClientError> {
        let path = self.dirs.state.join("client").join("origin");
        if let Some(bytes) = crate::state_file::read_bytes(&path)? {
            return Ok(String::from_utf8_lossy(&bytes).trim().to_owned());
        }
        let id = JobId::generate()?.to_string();
        crate::state_file::write_bytes(&path, id.as_bytes())?;
        Ok(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    job: JobId,
    machine: MachineName,
    name: Option<JobName>,
    from: Option<SentFrom>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentFrom {
    pub root: PathBuf,
    pub manifest: BlobId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EarlierEntry {
    job: JobId,
    machine: MachineName,
    name: Option<JobName>,
}

fn record(
    ctx: &Context,
    submitted: &[Submitted],
    from: Option<&SentFrom>,
) -> Result<(), ClientError> {
    let path = ctx.index_path();
    let mut file = crate::state_file::open_append(&path)?;
    for item in submitted {
        let entry = IndexEntry {
            job: item.job.spec.id.clone(),
            machine: item.machine.name.clone(),
            name: item.job.spec.name.clone(),
            from: from.cloned(),
        };
        let mut line = serde_json::to_vec(&entry).map_err(|source| ClientError::Index {
            path: path.clone(),
            source,
        })?;
        line.push(b'\n');
        file.write_all(&line).map_err(io("writing", &path))?;
    }
    Ok(())
}

fn index(ctx: &Context) -> Result<Vec<IndexEntry>, ClientError> {
    let path = ctx.index_path();
    let text = match crate::state_file::read_bytes(&path)? {
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => return Ok(Vec::new()),
    };
    Ok(readable_lines(&text))
}

fn readable_lines(text: &str) -> Vec<IndexEntry> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match crate::ingress::json_text(line) {
            Ok(entry) => Some(entry),
            Err(_torn_foreign_or_earlier) => {
                match crate::ingress::json_text::<EarlierEntry>(line) {
                    Ok(EarlierEntry { job, machine, name }) => Some(IndexEntry {
                        job,
                        machine,
                        name,
                        from: None,
                    }),
                    Err(_torn_or_foreign) => None,
                }
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Wanted {
    Latest,
    Id(JobRef),
    Named(JobName),
}

fn wanted(text: &str) -> Result<Wanted, ClientError> {
    if text == "latest" {
        return Ok(Wanted::Latest);
    }
    match JobRef::parse_loose(text) {
        Ok(reference) => Ok(Wanted::Id(reference)),
        Err(not_an_id) => match text.parse::<JobName>() {
            Ok(name) => Ok(Wanted::Named(name)),
            Err(_not_a_name) => Err(not_an_id.into()),
        },
    }
}

fn newest<'a>(
    entries: &'a [IndexEntry],
    machine: Option<&MachineName>,
    keep: impl Fn(&IndexEntry) -> bool,
) -> Option<&'a IndexEntry> {
    entries
        .iter()
        .rev()
        .filter(|e| machine.is_none_or(|m| &e.machine == m))
        .find(|e| keep(e))
}

pub fn locate(ctx: &Context, text: &str) -> Result<(Machine, JobRef), ClientError> {
    let (machine, rest) = match text.rsplit_once(':') {
        Some((machine, rest)) => (Some(machine.parse::<MachineName>()?), rest),
        None => (None, text),
    };
    let entries = index(ctx)?;
    let found = |entry: Option<&IndexEntry>| -> Result<(Machine, JobRef), ClientError> {
        let entry = entry.ok_or_else(|| ClientError::Unknown(text.to_owned()))?;
        Ok((
            ctx.config.machine(&entry.machine)?,
            entry.job.clone().into(),
        ))
    };
    if let Ok(name) = rest.parse::<JobName>()
        && let Some(entry) = newest(&entries, machine.as_ref(), |e| {
            e.name.as_ref() == Some(&name)
        })
    {
        return found(Some(entry));
    }
    match (wanted(rest)?, machine) {
        (Wanted::Latest, machine) => found(newest(&entries, machine.as_ref(), |_| true)),
        (Wanted::Named(name), machine) => found(newest(&entries, machine.as_ref(), |e| {
            e.name.as_ref() == Some(&name)
        })),
        (Wanted::Id(reference), Some(machine)) => Ok((ctx.config.machine(&machine)?, reference)),
        (Wanted::Id(reference), None) => {
            let mut matches: Vec<&IndexEntry> = entries
                .iter()
                .filter(|e| e.job.matches(&reference))
                .collect();
            matches.dedup_by(|a, b| a.machine == b.machine);
            match matches.as_slice() {
                [only] => Ok((ctx.config.machine(&only.machine)?, reference)),
                [] => match rest.parse::<JobName>() {
                    Ok(name) => found(newest(&entries, None, |e| e.name.as_ref() == Some(&name))),
                    Err(_not_a_name) => Err(ClientError::Unknown(text.to_owned())),
                },
                many => Err(ClientError::Ambiguous {
                    reference: text.to_owned(),
                    candidates: many
                        .iter()
                        .map(|e| {
                            let id = e.job.as_str();
                            let short = id.get(..crate::ui::SHORT_ID).unwrap_or(id);
                            match &e.name {
                                Some(name) => format!("{}:{short} ({name})", e.machine),
                                None => format!("{}:{short}", e.machine),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                }),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sending {
    Directory,
    Nothing,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub targets: String,
    pub words: Vec<Arg>,
    pub runner: Option<String>,
    pub rev: Option<crate::domain::Revision>,
    pub sending: Sending,
    pub workspace: Workspace,
    pub start: PathBuf,
    pub root: Option<PathBuf>,
    pub env: BTreeMap<EnvName, String>,
    pub shell: Option<String>,
    pub name: Option<JobName>,
    pub queue: crate::protocol::Queue,
}

#[derive(Debug, Clone)]
pub struct Submitted {
    pub machine: Machine,
    pub job: Job,
}

#[derive(Debug)]
pub struct Rejected {
    pub machine: MachineName,
    pub error: RemoteError,
}

fn command(config: &Config, order: &Order) -> Result<Command, ClientError> {
    let runner_name = order.runner.clone().unwrap_or_else(|| {
        if order.words.len() == 1 {
            "shell".to_owned()
        } else {
            "exec".to_owned()
        }
    });
    if order.words.is_empty() {
        return Err(ClientError::NoInput);
    }
    let bindings = Bindings::new()
        .with("input", Arg::spaced(&order.words))
        .with_list("words", order.words.clone());
    let error = |source| ClientError::Runner {
        runner: runner_name.clone(),
        source,
    };
    match config.runner(&runner_name)? {
        RunnerConf::Script { run } => Ok(Command::Script(
            run.render(&bindings).map_err(error)?.into_string(),
        )),
        RunnerConf::Argv { run } => Ok(Command::Argv(
            run.render(&bindings)
                .map_err(error)?
                .into_iter()
                .map(Arg::into_string)
                .collect(),
        )),
    }
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub root: PathBuf,
    pub snapshot: Snapshot,
    pub manifest: (BlobId, Vec<u8>),
    pub source: Source,
    pub subdir: Option<RelPath>,
}

fn project_root(order: &Order, config: &Config) -> Result<PathBuf, ClientError> {
    root_of(&order.start, order.root.as_deref(), config)
}

fn root_of(start: &Path, given: Option<&Path>, config: &Config) -> Result<PathBuf, ClientError> {
    if let Some(given) = given {
        return Ok(given.to_path_buf());
    }
    if let Some(found) = project::find_root(start) {
        return Ok(found);
    }
    Ok(snapshot::detect(config, start)?.map_or_else(|| start.to_path_buf(), |found| found.root))
}

pub fn project_here(
    ctx: &Context,
    start: &Path,
    root: Option<&Path>,
) -> Result<(PathBuf, ProjectKey), ClientError> {
    let start = std::fs::canonicalize(start).map_err(io("resolving", start))?;
    let found = root_of(&start, root, &ctx.config)?;
    let found = std::fs::canonicalize(&found).map_err(io("resolving", &found))?;
    let (place, named) = repository_place(ctx, &found);
    let key = project_key(&ctx.origin()?, &place, &named)?;
    Ok((found, key))
}

fn project_key(
    origin: &str,
    root: &Path,
    named: &std::ffi::OsStr,
) -> Result<ProjectKey, ClientError> {
    let name: String = named
        .to_string_lossy()
        .trim_end_matches(".git")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .skip_while(|c| *c == '-')
        .take(60)
        .collect();
    let digest = blake3::hash(format!("{origin}\0{}", root.display()).as_bytes()).to_hex();
    let short: String = digest.chars().take(12).collect();
    Ok(format!("{}-{short}", if name.is_empty() { "root" } else { &name }).parse()?)
}

pub fn env_map(
    pairs: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<EnvName, String>, ClientError> {
    let mut out = BTreeMap::new();
    for (key, value) in pairs.into_iter().flatten() {
        out.insert(key.parse::<EnvName>()?, value.clone());
    }
    Ok(out)
}

fn repository_place(ctx: &Context, root: &Path) -> (PathBuf, std::ffi::OsString) {
    let plain = || {
        (
            root.to_path_buf(),
            root.file_name().unwrap_or(root.as_os_str()).to_owned(),
        )
    };
    let Ok(Some(detected)) = snapshot::detect(&ctx.config, root) else {
        return plain();
    };
    let Some(shared) = snapshot::identity(&detected) else {
        return plain();
    };
    let named = repository_name(&shared).unwrap_or_else(|| root.as_os_str().to_owned());
    let place = match root.strip_prefix(&detected.root) {
        Ok(inner) if !inner.as_os_str().is_empty() => shared.join(inner),
        Ok(_) | Err(_) => shared,
    };
    (place, named)
}

fn repository_name(shared: &Path) -> Option<std::ffi::OsString> {
    let parts: Vec<&std::ffi::OsStr> = shared.iter().collect();
    parts
        .iter()
        .rposition(|part| part.to_string_lossy().starts_with('.'))
        .and_then(|dot| dot.checked_sub(1))
        .and_then(|before| parts.get(before))
        .or_else(|| parts.last())
        .map(|part| (*part).to_owned())
}

pub fn prepared(
    ctx: &Context,
    root: &Path,
    snapshot: Snapshot,
    subdir: Option<RelPath>,
) -> Result<Prepared, ClientError> {
    let (place, named) = repository_place(ctx, root);
    let project = project_key(&ctx.origin()?, &place, &named)?;
    let manifest = snapshot.manifest.encode()?;
    let source = Source {
        project,
        manifest: manifest.0.clone(),
        revision: snapshot.revision.clone(),
    };
    Ok(Prepared {
        root: root.to_path_buf(),
        snapshot,
        manifest,
        source,
        subdir,
    })
}

pub fn prepare(ctx: &Context, order: &Order) -> Result<Option<Prepared>, ClientError> {
    if order.sending == Sending::Nothing {
        return Ok(None);
    }
    let start = std::fs::canonicalize(&order.start).map_err(io("resolving", &order.start))?;
    let order = Order {
        start,
        ..order.clone()
    };
    let root = std::fs::canonicalize(project_root(&order, &ctx.config)?)
        .map_err(io("resolving", &order.start))?;
    let snapshot = match &order.rev {
        None => snapshot::from_directory(&root)?,
        Some(rev) => {
            let detected = snapshot::detect(&ctx.config, &root)?
                .ok_or_else(|| SnapshotError::NoSource(root.clone()))?;
            snapshot::from_revision(&detected, rev)?
        }
    };
    let subdir = match order.start.strip_prefix(&root) {
        Ok(inner) if inner.as_os_str().is_empty() => None,
        Ok(_) => Some(snapshot::relative(&root, &order.start)?),
        Err(_outside) => None,
    };
    prepared(ctx, &root, snapshot, subdir).map(Some)
}

fn deliver(
    link: &Link<'_>,
    prepared: &Prepared,
    report: &dyn Fn(Stage<'_>),
) -> Result<(), RemoteError> {
    let mut wanted = prepared.snapshot.manifest.blobs();
    wanted.push(prepared.manifest.0.clone());
    let missing = link
        .call(&Request::Missing { blobs: wanted }, &[])?
        .into_missing()
        .map_err(|other| link.unexpected("missing", *other))?;
    if missing.is_empty() {
        report(Stage::UpToDate);
        return Ok(());
    }
    let manifest_origin = Origin::Memory(prepared.manifest.1.clone());
    let payload: Vec<(&BlobId, &Origin)> = missing
        .iter()
        .filter_map(|blob| {
            if *blob == prepared.manifest.0 {
                Some((blob, &manifest_origin))
            } else {
                prepared
                    .snapshot
                    .origins
                    .get(blob)
                    .map(|origin| (blob, origin))
            }
        })
        .collect();
    let count = crate::domain::len_u64(payload.len());
    let bytes = payload.iter().fold(0u64, |sum, (blob, origin)| {
        let size = match origin {
            Origin::Memory(bytes) => crate::domain::len_u64(bytes.len()),
            Origin::Disk(_) => prepared
                .snapshot
                .manifest
                .entries
                .values()
                .find_map(|entry| match entry {
                    Entry::File {
                        blob: known, size, ..
                    } if known == *blob => Some(*size),
                    Entry::File { .. } | Entry::Symlink { .. } => None,
                })
                .unwrap_or(0),
        };
        sum.saturating_add(size)
    });
    report(Stage::Sending {
        files: count,
        bytes,
    });
    link.call(&Request::Upload { count }, &payload)?
        .into_stored()
        .map(drop)
        .map_err(|other| link.unexpected("stored", *other))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage<'a> {
    Connecting,
    Connected,
    Sending { files: u64, bytes: u64 },
    UpToDate,
    Submitted(&'a Job),
}

const SUBMIT_ATTEMPTS: u32 = 3;

pub type Report<'a> = &'a (dyn Fn(&MachineName, Stage<'_>) + Sync);

struct Plan<'a> {
    ctx: &'a Context,
    order: &'a Order,
    command: Command,
    prepared: Option<Prepared>,
    report: Report<'a>,
}

impl Plan<'_> {
    fn submission(&self, machine: &Machine, nonce: &crate::domain::Nonce) -> Submission {
        let location = match &self.prepared {
            Some(prepared) => Location::Snapshot {
                source: prepared.source.clone(),
                subdir: prepared.subdir.clone(),
                workspace: self.order.workspace,
            },
            None => Location::Home,
        };
        Submission {
            nonce: nonce.clone(),
            name: self.order.name.clone(),
            command: self.command.clone(),
            location,
            env: self.order.env.clone(),
            shell: self.order.shell.clone().or_else(|| machine.shell.clone()),
            concurrency: machine.max_jobs,
            queue: self.order.queue,
        }
    }

    fn one(&self, machine: &Machine) -> Result<Submitted, RemoteError> {
        let report = |stage: Stage<'_>| (self.report)(&machine.name, stage);
        report(Stage::Connecting);
        let nonce = crate::domain::Nonce::generate().map_err(|e| RemoteError::Io {
            action: "choosing a nonce for",
            path: PathBuf::from(machine.name.as_str()),
            source: std::io::Error::other(e.to_string()),
        })?;
        let mut link = Link::open(&self.ctx.config, &self.ctx.dirs, machine)?;
        report(Stage::Connected);
        let mut attempts_left = SUBMIT_ATTEMPTS;
        let job = loop {
            attempts_left = attempts_left.saturating_sub(1);
            let sent = self
                .prepared
                .as_ref()
                .map_or(Ok(()), |prepared| deliver(&link, prepared, &report))
                .and_then(|()| {
                    link.call(
                        &Request::Submit {
                            submission: Box::new(self.submission(machine, &nonce)),
                        },
                        &[],
                    )
                });
            let reply = match sent {
                Ok(reply) => reply,
                Err(error) if transient(&error) && attempts_left > 0 => {
                    eprintln!("domyjob: {error}; submitting again, which cannot start it twice");
                    link = Link::open(&self.ctx.config, &self.ctx.dirs, machine)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let missing = matches!(
                &reply,
                Reply::Refused(refusal)
                    if refusal.code == crate::protocol::RefusalCode::MissingContent
            );
            if !missing || attempts_left == 0 {
                break reply
                    .into_job()
                    .map_err(|other| link.unexpected("job", *other))?;
            }
        };
        let submitted = Submitted {
            machine: machine.clone(),
            job,
        };
        let from = self.prepared.as_ref().map(|prepared| SentFrom {
            root: prepared.root.clone(),
            manifest: prepared.manifest.0.clone(),
        });
        if let Err(error) = record(self.ctx, std::slice::from_ref(&submitted), from.as_ref()) {
            eprintln!(
                "domyjob: {}: submitted {} but could not note it on this machine, so `latest` and its name will not find it: {error}",
                machine.name, submitted.job.spec.id
            );
        }
        report(Stage::Submitted(&submitted.job));
        Ok(submitted)
    }
}

fn across<T: Send>(
    machines: &[Machine],
    work: impl Fn(&Machine) -> Result<T, RemoteError> + Sync,
) -> Vec<(MachineName, Result<T, RemoteError>)> {
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(machines.len());
        for machine in machines {
            let work = &work;
            handles.push((machine.name.clone(), scope.spawn(move || work(machine))));
        }
        let mut out = Vec::with_capacity(handles.len());
        for (name, handle) in handles {
            let result = match handle.join() {
                Ok(result) => result,
                Err(_panicked) => Err(RemoteError::Probe {
                    machine: name.to_string(),
                    detail: "a worker thread panicked".to_owned(),
                }),
            };
            out.push((name, result));
        }
        out
    })
}

pub fn submit(
    ctx: &Context,
    order: &Order,
    report: Report<'_>,
) -> Result<(Vec<Submitted>, Vec<Rejected>), ClientError> {
    let prepared = prepare(ctx, order)?;
    submit_prepared(ctx, order, prepared, report)
}

pub const fn quietly(_machine: &MachineName, _stage: Stage<'_>) {}

pub fn submit_prepared(
    ctx: &Context,
    order: &Order,
    prepared: Option<Prepared>,
    report: Report<'_>,
) -> Result<(Vec<Submitted>, Vec<Rejected>), ClientError> {
    let machines = ctx.select(&order.targets)?;
    let plan = Plan {
        ctx,
        order,
        command: command(&ctx.config, order)?,
        prepared,
        report,
    };
    let results = across(&machines, |machine| plan.one(machine));
    let mut submitted = Vec::new();
    let mut rejected = Vec::new();
    for (machine, result) in results {
        match result {
            Ok(item) => submitted.push(item),
            Err(error) => rejected.push(Rejected { machine, error }),
        }
    }
    Ok((submitted, rejected))
}

#[derive(Debug, Clone)]
pub struct Preview {
    pub machines: Vec<MachineName>,
    pub command: Command,
    pub sending: Option<Sent>,
}

#[derive(Debug, Clone)]
pub struct Sent {
    pub root: PathBuf,
    pub subdir: Option<RelPath>,
    pub revision: String,
    pub files: u64,
    pub bytes: u64,
}

pub fn preview(ctx: &Context, order: &Order) -> Result<Preview, ClientError> {
    let machines = ctx
        .select(&order.targets)?
        .into_iter()
        .map(|m| m.name)
        .collect();
    let sending = prepare(ctx, order)?.map(|prepared| {
        let (files, bytes) = prepared.snapshot.manifest.entries.values().fold(
            (0u64, 0u64),
            |(files, bytes), entry| match entry {
                Entry::File { size, .. } => (files.saturating_add(1), bytes.saturating_add(*size)),
                Entry::Symlink { .. } => (files.saturating_add(1), bytes),
            },
        );
        Sent {
            root: prepared.root,
            subdir: prepared.subdir,
            revision: prepared.source.revision.describe(),
            files,
            bytes,
        }
    });
    Ok(Preview {
        machines,
        command: command(&ctx.config, order)?,
        sending,
    })
}

#[must_use]
pub fn examine(
    ctx: &Context,
    machines: &[Machine],
) -> Vec<(MachineName, Result<crate::remote::Facts, RemoteError>)> {
    across(machines, |machine| examine_one(ctx, machine))
}

pub fn examine_one(ctx: &Context, machine: &Machine) -> Result<crate::remote::Facts, RemoteError> {
    Link::open(&ctx.config, &ctx.dirs, machine)?;
    cached_facts(&ctx.dirs, machine)?.map_or_else(
        || {
            Ok(crate::remote::Facts {
                hello: crate::node::hello(&ctx.dirs),
                placement: crate::remote::Placement::Installed,
            })
        },
        Ok,
    )
}

pub fn known_machines(ctx: &Context) -> Result<Vec<Machine>, ClientError> {
    let mut names: Vec<MachineName> = ctx.config.machines.keys().cloned().collect();
    names.extend(index(ctx)?.into_iter().map(|e| e.machine));
    names.sort();
    names.dedup();
    let mut machines = Vec::new();
    for name in names {
        let Ok(machine) = ctx.config.machine(&name) else {
            continue;
        };
        if cached_facts(&ctx.dirs, &machine)?.is_some() || machine.transport == "local" {
            machines.push(machine);
        }
    }
    Ok(machines)
}

pub fn clean(
    ctx: &Context,
    machine: &Machine,
    (apply, logs, idle): (bool, bool, bool),
) -> Result<crate::protocol::Cleaned, RemoteError> {
    let link = Link::open(&ctx.config, &ctx.dirs, machine)?;
    link.call(&Request::Clean { apply, logs, idle }, &[])?
        .into_cleaned()
        .map_err(|other| link.unexpected("what was cleaned", *other))
}

pub fn pause(
    ctx: &Context,
    machine: &Machine,
    paused: bool,
) -> Result<crate::protocol::Report, RemoteError> {
    let link = Link::open(&ctx.config, &ctx.dirs, machine)?;
    link.call(&Request::Pause { paused }, &[])?
        .into_report()
        .map_err(|other| link.unexpected("a report", *other))
}

struct Surveys<'a> {
    pending: Vec<u8>,
    each: &'a mut dyn FnMut(crate::protocol::Survey),
}

impl Write for Surveys<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let survey = crate::ingress::json(&line).map_err(std::io::Error::other)?;
            (self.each)(survey);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn watch(
    ctx: &Context,
    machine: &Machine,
    each: &mut dyn FnMut(crate::protocol::Survey),
) -> Result<(), RemoteError> {
    let link = Link::open(&ctx.config, &ctx.dirs, machine)?;
    let mut surveys = Surveys {
        pending: Vec::new(),
        each,
    };
    link.stream(&Request::Watch, &mut surveys)?
        .into_stream()
        .map_err(|other| link.unexpected("a stream of surveys", *other))
}

pub fn survey(
    ctx: &Context,
    machine: &Machine,
) -> Result<(crate::protocol::Report, Vec<Job>), RemoteError> {
    let link = Link::open(&ctx.config, &ctx.dirs, machine)?;
    let report = link
        .call(&Request::Report, &[])?
        .into_report()
        .map_err(|other| link.unexpected("a report", *other))?;
    let (jobs, _unreadable) = link
        .call(&Request::List { limit: 50 }, &[])?
        .into_jobs()
        .map_err(|other| link.unexpected("jobs", *other))?;
    Ok((report, jobs))
}

#[must_use]
pub fn list(
    ctx: &Context,
    machines: &[Machine],
    limit: u32,
) -> (Vec<(MachineName, Job)>, Vec<Rejected>) {
    let results = across(machines, |machine| {
        let link = Link::open(&ctx.config, &ctx.dirs, machine)?;
        link.call(&Request::List { limit }, &[])?
            .into_jobs()
            .map_err(|other| link.unexpected("jobs", *other))
    });
    let mut jobs = Vec::new();
    let mut rejected = Vec::new();
    for (machine, result) in results {
        match result {
            Ok((found, unreadable)) => {
                jobs.extend(found.into_iter().map(|job| (machine.clone(), job)));
                rejected.extend(unreadable.into_iter().map(|bad| Rejected {
                    machine: machine.clone(),
                    error: RemoteError::Unreadable {
                        machine: machine.to_string(),
                        job: bad.id,
                        why: bad.why,
                    },
                }));
            }
            Err(error) => rejected.push(Rejected { machine, error }),
        }
    }
    jobs.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.spec.sequence.cmp(&a.1.spec.sequence))
    });
    (jobs, rejected)
}

pub fn job_request(
    ctx: &Context,
    reference: &str,
    request: impl Fn(JobRef) -> Request,
) -> Result<(Machine, Job), ClientError> {
    ask(ctx, reference, request, ("job", Reply::into_job))
}

type Take<T> = (&'static str, fn(Reply) -> Result<T, Box<Reply>>);

const TRANSPORT_ATTEMPTS: u32 = 2;

const fn transient(error: &RemoteError) -> bool {
    matches!(
        error,
        RemoteError::Silent { .. }
            | RemoteError::Pipe { .. }
            | RemoteError::Exited { .. }
            | RemoteError::Stream {
                problem: crate::framed::Unframed::Truncated | crate::framed::Unframed::Reading(_),
                ..
            }
    )
}

fn ask<T>(
    ctx: &Context,
    reference: &str,
    request: impl Fn(JobRef) -> Request,
    (expected, take): Take<T>,
) -> Result<(Machine, T), ClientError> {
    let (machine, job) = locate(ctx, reference)?;
    let mut attempts_left = TRANSPORT_ATTEMPTS;
    loop {
        attempts_left = attempts_left.saturating_sub(1);
        let link = Link::open(&ctx.config, &ctx.dirs, &machine)?;
        match link.call(&request(job.clone()), &[]) {
            Ok(reply) => {
                let answer = take(reply).map_err(|other| link.unexpected(expected, *other))?;
                return Ok((machine, answer));
            }
            Err(error) if transient(&error) && attempts_left > 0 => {
                eprintln!("domyjob: {error}; asking again");
            }
            Err(error) => return Err(error.into()),
        }
    }
}

struct Delivered<'a> {
    sink: &'a mut dyn Write,
    bytes: u64,
}

impl Write for Delivered<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.sink.write(buf)?;
        self.bytes = self.bytes.saturating_add(crate::domain::len_u64(written));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

pub fn digest(
    ctx: &Context,
    reference: &str,
    tail: u32,
) -> Result<(Machine, crate::protocol::Digest), ClientError> {
    ask(
        ctx,
        reference,
        |job| Request::Digest { job, tail },
        ("a digest", Reply::into_digest),
    )
}

#[derive(Debug, Clone)]
pub struct Query {
    pub pattern: String,
    pub context: u32,
    pub limit: u32,
}

pub fn search(
    ctx: &Context,
    reference: &str,
    query: Query,
) -> Result<(Machine, crate::protocol::Found), ClientError> {
    let Query {
        pattern,
        context,
        limit,
    } = query;
    ask(
        ctx,
        reference,
        |job| Request::Search {
            job,
            pattern: pattern.clone(),
            context,
            limit,
        },
        ("search results", Reply::into_found),
    )
}

pub fn wait(ctx: &Context, reference: &str) -> Result<(Machine, Job), ClientError> {
    job_request(ctx, reference, |job| Request::Wait { job })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Follow,
    Snapshot,
    Tail(u32),
}

pub fn logs(
    ctx: &Context,
    reference: &str,
    output: Output,
    sink: &mut dyn Write,
) -> Result<Machine, ClientError> {
    let (machine, job) = locate(ctx, reference)?;
    let mut delivered = Delivered { sink, bytes: 0 };
    let mut attempts_left = TRANSPORT_ATTEMPTS;
    loop {
        attempts_left = attempts_left.saturating_sub(1);
        let offset = delivered.bytes;
        let request = match output {
            Output::Follow => Request::Logs {
                job: job.clone(),
                offset,
                follow: Follow::UntilFinished,
            },
            Output::Snapshot => Request::Logs {
                job: job.clone(),
                offset,
                follow: Follow::Snapshot,
            },
            Output::Tail(lines) => Request::Tail {
                job: job.clone(),
                lines,
            },
        };
        let resumable = !matches!(output, Output::Tail(_)) || delivered.bytes == 0;
        let link = Link::open(&ctx.config, &ctx.dirs, &machine)?;
        match link.stream(&request, &mut delivered) {
            Ok(reply) => {
                reply
                    .into_stream()
                    .map_err(|other| link.unexpected("a log stream", *other))?;
                return Ok(machine);
            }
            Err(error) if transient(&error) && resumable && attempts_left > 0 => {
                eprintln!("domyjob: {error}; picking up where it stopped");
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn get(
    ctx: &Context,
    reference: &str,
    path: RelPath,
    sink: &mut dyn Write,
) -> Result<Machine, ClientError> {
    let (machine, job) = locate(ctx, reference)?;
    let link = Link::open(&ctx.config, &ctx.dirs, &machine)?;
    link.stream(&Request::Get { job, path }, sink)?
        .into_stream()
        .map_err(|other| link.unexpected("a file", *other))?;
    Ok(machine)
}

#[derive(Debug)]
pub struct Pulled {
    pub machine: Machine,
    pub job: Job,
    pub from: SentFrom,
    pub plan: crate::pull::Plan<crate::pull::Forward>,
}

pub fn changes(ctx: &Context, reference: &str) -> Result<Pulled, ClientError> {
    let (machine, job_ref) = locate(ctx, reference)?;
    let link = Link::open(&ctx.config, &ctx.dirs, &machine)?;
    let job = link
        .call(
            &Request::Status {
                job: job_ref.clone(),
            },
            &[],
        )?
        .into_job()
        .map_err(|other| link.unexpected("a job", *other))?;
    let from = sent_from(ctx, &machine.name, &job.spec.id)?;
    let mut payload = Vec::new();
    link.stream(&Request::Changes { job: job_ref }, &mut payload)?
        .into_stream()
        .map_err(|other| link.unexpected("changes", *other))?;
    let plan = unpack(&payload, &from.manifest).map_err(|why| ClientError::Unpacked {
        machine: machine.name.clone(),
        why,
    })?;
    Ok(Pulled {
        machine,
        job,
        from,
        plan,
    })
}

pub fn recorded(ctx: &Context, text: &str) -> Result<(MachineName, JobId), ClientError> {
    let (machine, reference) = locate(ctx, text)?;
    index(ctx)?
        .into_iter()
        .rev()
        .find(|entry| entry.machine == machine.name && entry.job.matches(&reference))
        .map(|entry| (entry.machine, entry.job))
        .ok_or_else(|| ClientError::Unknown(text.to_owned()))
}

#[must_use]
pub fn pulls(ctx: &Context) -> PathBuf {
    ctx.dirs.state.join("client").join("pulls")
}

fn sent_from(ctx: &Context, machine: &MachineName, job: &JobId) -> Result<SentFrom, ClientError> {
    index(ctx)?
        .into_iter()
        .rev()
        .find(|entry| &entry.job == job && &entry.machine == machine)
        .and_then(|entry| entry.from)
        .ok_or_else(|| ClientError::NotSent { job: job.clone() })
}

#[derive(Debug, thiserror::Error)]
pub enum Unpacking {
    #[error("the list of changes is missing")]
    NoList,
    #[error("the list of changes is not valid: {0}")]
    List(serde_json::Error),
    #[error("the list of what was sent is cut short")]
    NoManifest,
    #[error("{0} is too large for this machine")]
    TooLarge(RelPath),
    #[error("{0} was cut short")]
    Short(RelPath),
    #[error("more arrived than the list of changes describes")]
    Extra,
    #[error(transparent)]
    Malformed(#[from] crate::pull::Malformed),
}

fn unpack(
    payload: &[u8],
    recorded: &BlobId,
) -> Result<crate::pull::Plan<crate::pull::Forward>, Unpacking> {
    let end = payload
        .iter()
        .position(|b| *b == b'\n')
        .ok_or(Unpacking::NoList)?;
    let (list, after_list) = payload.split_at(end);
    let after_list = after_list.get(1..).unwrap_or_default();
    let header: snapshot::Changed = crate::ingress::json(list).map_err(Unpacking::List)?;
    let split = match usize::try_from(header.sent) {
        Ok(size) => after_list.split_at_checked(size),
        Err(_too_large) => None,
    };
    let (raw, mut rest) = split.ok_or(Unpacking::NoManifest)?;
    let sent = crate::pull::SentManifest::verified(raw, recorded)?;
    let mut contents = BTreeMap::new();
    for left in &header.left {
        if let Some(Entry::File { size, .. }) = &left.now {
            let wanted = usize::try_from(*size)
                .map_err(|_too_large| Unpacking::TooLarge(left.path.clone()))?;
            let (bytes, next) = rest
                .split_at_checked(wanted)
                .ok_or_else(|| Unpacking::Short(left.path.clone()))?;
            contents.insert(left.path.clone(), bytes.to_vec());
            rest = next;
        }
    }
    if !rest.is_empty() {
        return Err(Unpacking::Extra);
    }
    Ok(crate::pull::Plan::new(&sent, header.left, contents)?)
}

impl crate::ingress::Ingress for IndexEntry {}
impl crate::ingress::Ingress for EarlierEntry {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_torn_or_foreign_index_line_is_skipped_not_fatal() {
        let good = r#"{"job":"0AAAAAAAAAAAAAAA","machine":"linux","name":null}"#;
        let text = format!("{good}\n{{\"job\":\"0BBB\n\nnot json\n{good}\n{{\"job\"");
        let entries = readable_lines(&text);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn changes_unpack_only_against_the_list_this_machine_sent() {
        let entry = |text: &[u8]| Entry::File {
            blob: BlobId::of(text),
            size: crate::domain::len_u64(text.len()),
            mode: snapshot::Mode::Regular,
        };
        let sent = snapshot::Manifest {
            entries: BTreeMap::from([
                ("gone.txt".parse().unwrap(), entry(b"x")),
                ("same.txt".parse().unwrap(), entry(b"same")),
            ]),
        };
        let (recorded, raw) = sent.encode().unwrap();
        let left = |path: &str, now: Option<Entry>| snapshot::Left {
            path: path.parse().unwrap(),
            now,
        };
        let changed = snapshot::Changed {
            sent: crate::domain::len_u64(raw.len()),
            left: vec![
                left("a.txt", Some(entry(b"alpha"))),
                left("b.txt", Some(entry(b"beta"))),
                left("gone.txt", None),
                left("same.txt", Some(entry(b"same"))),
            ],
        };
        let payload = |header: &snapshot::Changed, listed: &[u8], tail: &[u8]| {
            let mut bytes = serde_json::to_vec(header).unwrap();
            bytes.push(b'\n');
            bytes.extend_from_slice(listed);
            bytes.extend_from_slice(tail);
            bytes
        };
        let plan = unpack(&payload(&changed, &raw, b"alphabetasame"), &recorded).unwrap();
        let kinds: String = plan.steps().iter().map(|s| s.kind().letter()).collect();
        assert_eq!(kinds, "AAD");
        assert!(matches!(
            unpack(&payload(&changed, &raw, b"alphabetasame!"), &recorded),
            Err(Unpacking::Extra)
        ));
        assert!(matches!(
            unpack(&payload(&changed, &raw, b"alphabet"), &recorded),
            Err(Unpacking::Short(_))
        ));
        assert!(matches!(
            unpack(&payload(&changed, &raw, b"alphaBETAsame"), &recorded),
            Err(Unpacking::Malformed(crate::pull::Malformed::Damaged(_)))
        ));
        assert!(matches!(
            unpack(
                &payload(&changed, &raw, b"alphabetasame"),
                &BlobId::of(b"other")
            ),
            Err(Unpacking::Malformed(crate::pull::Malformed::OtherManifest))
        ));
        let mut twice = changed.clone();
        twice.left.swap(0, 1);
        assert!(matches!(
            unpack(&payload(&twice, &raw, b"betaalphasame"), &recorded),
            Err(Unpacking::Malformed(crate::pull::Malformed::Unordered(_)))
        ));
        assert!(matches!(
            unpack(&payload(&changed, raw.get(..3).unwrap(), b""), &recorded),
            Err(Unpacking::NoManifest)
        ));
        assert!(matches!(unpack(b"{}", &recorded), Err(Unpacking::NoList)));
        assert!(matches!(unpack(b"{\n", &recorded), Err(Unpacking::List(_))));
    }

    #[test]
    fn a_repository_is_named_after_the_directory_that_holds_its_store() {
        for (shared, name) in [
            ("/x/njutest/.git", "njutest"),
            ("/x/proj/.jj/repo/config.toml", "proj"),
            ("/srv/hub.git", "hub.git"),
        ] {
            assert_eq!(
                repository_name(Path::new(shared)).unwrap(),
                std::ffi::OsStr::new(name)
            );
        }
        assert_eq!(repository_name(Path::new("")), None);
    }

    #[test]
    fn project_keys_are_stable_and_safe() {
        let named = std::ffi::OsStr::new("my project");
        let a = project_key("origin", Path::new("/work/my project"), named).unwrap();
        let b = project_key("origin", Path::new("/work/my project"), named).unwrap();
        let c = project_key("other", Path::new("/work/my project"), named).unwrap();
        let elsewhere = project_key("origin", Path::new("/work/elsewhere"), named).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, elsewhere);
        assert!(a.as_str().starts_with("my_project-"));
        assert!(
            project_key(
                "o",
                Path::new("/srv/hub.git"),
                std::ffi::OsStr::new("hub.git")
            )
            .unwrap()
            .as_str()
            .starts_with("hub-")
        );
    }

    #[test]
    fn runners_turn_words_into_commands() {
        let config = Config::layered(
            "[runners.agent]\nkind = \"argv\"\nrun = [\"agent\", \"-p\", \"{input}\"]\n",
            "t",
        )
        .unwrap();
        let order = |words: &[&str], runner: Option<&str>| Order {
            queue: crate::protocol::Queue::Slot,
            targets: "local".into(),
            words: words
                .iter()
                .map(|w| Arg::user(&crate::input::UserText::from_cli((*w).to_owned())))
                .collect(),
            runner: runner.map(str::to_owned),
            rev: None,
            sending: Sending::Nothing,
            workspace: Workspace::Warm,
            start: PathBuf::from("."),
            root: None,
            env: BTreeMap::new(),
            shell: None,
            name: None,
        };
        assert_eq!(
            command(&config, &order(&["make test"], None)).unwrap(),
            Command::Script("make test".into())
        );
        assert_eq!(
            command(&config, &order(&["cargo", "test"], None)).unwrap(),
            Command::Argv(vec!["cargo".into(), "test".into()])
        );
        assert_eq!(
            command(&config, &order(&["fix", "it"], Some("agent"))).unwrap(),
            Command::Argv(vec!["agent".into(), "-p".into(), "fix it".into()])
        );
        assert!(matches!(
            command(&config, &order(&[], None)),
            Err(ClientError::NoInput)
        ));
    }
}
