use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{self, ClientError, Context, Order, Output, Sending};
use crate::config::McpTool;
use crate::protocol::{Request, VERSION, Workspace};
use crate::template::Arg;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("reading from the client: {0}")]
    Input(std::io::Error),
    #[error("writing to the client: {0}")]
    Output(std::io::Error),
}

#[derive(Debug, thiserror::Error)]
enum ToolError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("arguments: {0}")]
    Arguments(serde_json::Error),
    #[error("no tool named {0}")]
    Unknown(String),
    #[error("the local [mcp] policy does not allow {0}")]
    NotAllowed(&'static str),
    #[error("{0} is not in the machines the local [mcp] policy allows")]
    Machine(String),
    #[error("{0} is not under a directory the local [mcp] policy allows")]
    Directory(String),
    #[error("{0:?} is not a revision")]
    Revision(String),
    #[error("writing {0}: {1}")]
    Destination(String, String),
    #[error("encoding the answer: {0}")]
    Encode(serde_json::Error),
}

fn answer<T: serde::Serialize>(body: &T) -> Result<Value, ToolError> {
    crate::output::value(body).map_err(ToolError::Encode)
}

fn part<T: serde::Serialize>(body: &T) -> Value {
    match serde_json::to_value(body) {
        Ok(value) => value,
        Err(error) => Value::String(format!("encoding the answer: {error}")),
    }
}

const fn tool_name(tool: McpTool) -> &'static str {
    match tool {
        McpTool::Machines => "machines",
        McpTool::ListJobs => "list_jobs",
        McpTool::JobStatus => "job_status",
        McpTool::JobLogs => "job_logs",
        McpTool::JobDigest => "job_digest",
        McpTool::SearchLogs => "search_logs",
        McpTool::WaitJob => "wait_job",
        McpTool::GetFile => "get_file",
        McpTool::Run => "run",
        McpTool::KillJob => "kill_job",
    }
}

fn tool_named(name: &str) -> Option<McpTool> {
    [
        McpTool::Machines,
        McpTool::ListJobs,
        McpTool::JobStatus,
        McpTool::JobLogs,
        McpTool::JobDigest,
        McpTool::SearchLogs,
        McpTool::WaitJob,
        McpTool::GetFile,
        McpTool::Run,
        McpTool::KillJob,
    ]
    .into_iter()
    .find(|tool| tool_name(*tool) == name)
}

fn permitted(ctx: &Context, args: &RunArgs) -> Result<(), ToolError> {
    let policy = &ctx.config.mcp;
    let allowed: Vec<crate::domain::MachineName> = if policy.machines.is_empty() {
        Vec::new()
    } else {
        ctx.select(&policy.machines)?
            .into_iter()
            .map(|m| m.name)
            .collect()
    };
    for machine in ctx.select(&args.machines)? {
        if !allowed.contains(&machine.name) {
            return Err(ToolError::Machine(machine.name.to_string()));
        }
    }
    let directory = std::fs::canonicalize(&args.directory)
        .map_err(|_missing| ToolError::Directory(args.directory.clone()))?;
    let mut inside = false;
    for allowed_dir in &policy.directories {
        match std::fs::canonicalize(allowed_dir) {
            Ok(resolved) => inside |= directory.starts_with(resolved),
            Err(_missing) => eprintln!(
                "domyjob: the [mcp] directory {} does not exist",
                allowed_dir.display()
            ),
        }
    }
    if !inside {
        return Err(ToolError::Directory(args.directory.clone()));
    }
    let runner = args.runner.clone().unwrap_or_else(|| {
        if args.command.len() == 1 {
            "shell".to_owned()
        } else {
            "exec".to_owned()
        }
    });
    if policy.runners.contains(&runner) {
        Ok(())
    } else {
        Err(ToolError::NotAllowed("that runner"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    machines: String,
    command: Vec<String>,
    directory: String,
    runner: Option<String>,
    rev: Option<String>,
    wait: Option<bool>,
    fresh: Option<bool>,
    name: Option<crate::domain::JobName>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DigestArgs {
    job: String,
    tail: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    job: String,
    pattern: String,
    context: Option<u32>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArgs {
    job: String,
    path: crate::domain::RelPath,
    destination: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobArgs {
    job: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogArgs {
    job: String,
    lines: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    machines: Option<String>,
    limit: Option<u32>,
}

const JOB: &str = "a job: its id, a unique prefix of one, MACHINE:ID, a name given with run's name, or latest (MACHINE:latest for one machine)";

fn object(required: &[&str], properties: &Value) -> Value {
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Reads,
    Writes,
    Destroys,
}

fn annotated(title: &str, effect: Effect) -> Value {
    let (read_only, destructive) = match effect {
        Effect::Reads => (true, false),
        Effect::Writes => (false, false),
        Effect::Destroys => (false, true),
    };
    json!({"title": title, "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": read_only, "openWorldHint": true})
}

fn tools() -> Value {
    let job = json!({"type": "string", "description": JOB});
    json!([
        {"name": "run",
         "description": "Send a local directory to machines and run a command there. The directory goes as it is on disk, uncommitted edits included; only files the machine lacks are uploaded, and build output stays warm between runs. Without wait it returns the job references at once and the jobs keep running; with wait it returns each job's digest when it finishes.",
         "annotations": annotated("Run a command on machines", Effect::Destroys),
         "inputSchema": object(&["machines", "command", "directory"], &json!({
            "machines": {"type": "string", "description": "where to run: a machine, labels joined with + such as windows+gpu, a fact such as os=linux, @group, or @all; comma separated"},
            "command": {"type": "array", "items": {"type": "string"}, "description": "one element is a script for the machine's shell; several are an argument vector run without a shell"},
            "directory": {"type": "string", "description": "absolute path of the local directory to send; the command runs in the same place inside it"},
            "wait": {"type": "boolean", "description": "stay until the jobs finish and return their digests"},
            "name": {"type": "string", "description": "a name to refer to the job by later, instead of its id"},
            "rev": {"type": "string", "description": "send a version-control revision such as HEAD or main instead of the directory as it is"},
            "runner": {"type": "string", "description": "a runner from the local configuration to hand the command to instead of a shell"},
            "fresh": {"type": "boolean", "description": "use an empty workspace that is deleted afterwards"}}))},
        {"name": "job_digest",
         "description": "The cheapest way to learn what a job did: its state, exit code, how long its log is, and its last lines, with terminal control sequences removed.",
         "annotations": annotated("Summarize a job", Effect::Reads),
         "inputSchema": object(&["job"], &json!({"job": job, "tail": {"type": "integer", "minimum": 0, "maximum": 1000, "description": "how many of the last lines to include (default 40)"}}))},
        {"name": "search_logs",
         "description": "Search a job's whole log on its machine with a regular expression and return only the matching lines, numbered, with context around them.",
         "annotations": annotated("Search a job's log", Effect::Reads),
         "inputSchema": object(&["job", "pattern"], &json!({"job": job, "pattern": {"type": "string", "description": "a regular expression (Rust regex syntax), at most 1024 bytes"}, "context": {"type": "integer", "minimum": 0, "maximum": 20}, "limit": {"type": "integer", "minimum": 1, "maximum": 1000}}))},
        {"name": "wait_job",
         "description": "Wait until a job finishes, then return its digest. Stopping this call does not stop the job; wait again or ask for its digest later.",
         "annotations": annotated("Wait for a job", Effect::Reads),
         "inputSchema": object(&["job"], &json!({"job": job}))},
        {"name": "job_status", "description": "Where a job stands, with its full specification.",
         "annotations": annotated("Show a job", Effect::Reads),
         "inputSchema": object(&["job"], &json!({"job": job}))},
        {"name": "job_logs", "description": "The last lines of a job's output as it was written. Prefer job_digest or search_logs, which cost fewer tokens.",
         "annotations": annotated("Read a job's log", Effect::Reads),
         "inputSchema": object(&["job"], &json!({"job": job, "lines": {"type": "integer", "minimum": 1}}))},
        {"name": "list_jobs", "description": "Recent jobs on machines, newest first per machine.",
         "annotations": annotated("List jobs", Effect::Reads),
         "inputSchema": object(&[], &json!({"machines": {"type": "string"}, "limit": {"type": "integer", "minimum": 1}}))},
        {"name": "get_file", "description": "Copy one file out of a job's workspace to a local path under a directory the local policy allows.",
         "annotations": annotated("Fetch a file from a job", Effect::Writes),
         "inputSchema": object(&["job", "path", "destination"], &json!({"job": job, "path": {"type": "string", "description": "relative to the directory the job ran in"}, "destination": {"type": "string", "description": "absolute local path of a file to create"}}))},
        {"name": "kill_job", "description": "Stop a job and every process it started, at once.",
         "annotations": annotated("Stop a job", Effect::Destroys),
         "inputSchema": object(&["job"], &json!({"job": job}))},
        {"name": "machines", "description": "The machines domyjob knows, with their labels and how they are reached.",
         "annotations": annotated("List machines", Effect::Reads),
         "inputSchema": object(&[], &json!({}))}
    ])
}

const INSTRUCTIONS: &str = "domyjob runs commands on other machines and keeps them running after you disconnect. Typical use: run with wait=false to start work, then job_digest or wait_job to learn the outcome, and search_logs to find the lines that matter. A job's log can be long; job_digest and search_logs return only what you ask for. Refer to jobs by id, by the name you gave them, or as latest. Jobs run in the environment of the machine, not yours: your ssh agent and local credentials are not available to them.";

fn parse<T: crate::ingress::Ingress>(arguments: &Value) -> Result<T, ToolError> {
    crate::ingress::json_value(arguments).map_err(ToolError::Arguments)
}

use crate::output::DIGEST_TAIL;

fn digested(ctx: &Context, reference: &str, tail: u32) -> Result<Value, ToolError> {
    let (machine, digest) = client::digest(ctx, reference, tail)?;
    Ok(part(&crate::output::DigestView::of(&machine.name, &digest)))
}

fn inside_allowed(ctx: &Context, destination: &str) -> Result<PathBuf, ToolError> {
    let path = PathBuf::from(destination);
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::Directory(destination.to_owned()))?;
    let resolved = std::fs::canonicalize(parent)
        .map_err(|_missing| ToolError::Directory(destination.to_owned()))?;
    let allowed = ctx
        .config
        .mcp
        .directories
        .iter()
        .any(|dir| std::fs::canonicalize(dir).is_ok_and(|root| resolved.starts_with(root)));
    match (allowed, path.file_name()) {
        (true, Some(name)) => Ok(resolved.join(name)),
        (true, None) | (false, _) => Err(ToolError::Directory(destination.to_owned())),
    }
}

fn tail(ctx: &Context, job: &str, lines: u32) -> Result<String, ToolError> {
    let mut buffer = Vec::new();
    client::logs(ctx, job, Output::Tail(lines), &mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn ran(ctx: &Context, item: &client::Submitted, waiting: bool) -> Value {
    if !waiting {
        return part(&crate::output::JobView::summary(
            &item.machine.name,
            &item.job,
        ));
    }
    let reference = format!("{}:{}", item.machine.name, item.job.spec.id);
    match client::wait(ctx, &reference)
        .map_err(ToolError::from)
        .and_then(|_| digested(ctx, &reference, DIGEST_TAIL))
    {
        Ok(digest) => digest,
        Err(error) => part(&crate::output::Unsettled {
            job: reference,
            machine: item.machine.name.clone(),
            state: crate::protocol::State::Lost.as_str(),
            error: crate::output::ErrorView::new(
                error.to_string(),
                crate::diagnosis::Diagnosis {
                    kind: crate::diagnosis::Kind::Unreachable,
                    hint: Some(
                        "it may still be running; job_status or wait_job asks again".to_owned(),
                    ),
                },
            ),
        }),
    }
}

fn run(ctx: &Context, args: RunArgs) -> Result<Value, ToolError> {
    permitted(ctx, &args)?;
    let rev = match &args.rev {
        Some(text) => Some(
            text.parse::<crate::domain::Revision>()
                .map_err(|_bad| ToolError::Revision(text.clone()))?,
        ),
        None => None,
    };
    let order = Order {
        queue: crate::protocol::Queue::Slot,
        targets: args.machines,
        words: args
            .command
            .into_iter()
            .map(|w| Arg::user(&crate::input::UserText::from_agent(w)))
            .collect(),
        runner: args.runner,
        rev,
        sending: Sending::Directory,
        workspace: if args.fresh == Some(true) {
            Workspace::Fresh
        } else {
            Workspace::Warm
        },
        start: PathBuf::from(&args.directory),
        root: None,
        env: std::collections::BTreeMap::new(),
        shell: None,
        name: args.name,
    };
    let (submitted, rejected) = client::submit(ctx, &order, &client::quietly)?;
    let waiting = args.wait == Some(true);
    let jobs: Vec<Value> = std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every wait must start before the first is joined, or they run one after another"
        )]
        let handles: Vec<_> = submitted
            .iter()
            .map(|item| scope.spawn(move || ran(ctx, item, waiting)))
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(entry) => entry,
                Err(_panicked) => Value::String("a worker panicked".to_owned()),
            })
            .collect()
    });
    answer(&crate::output::Jobs {
        jobs,
        unreachable: rejected
            .iter()
            .map(|r| crate::output::MachineError::of(&r.machine, &r.error))
            .collect(),
    })
}

fn list_jobs(ctx: &Context, args: &ListArgs) -> Result<Value, ToolError> {
    let machines = match &args.machines {
        Some(selector) => ctx.select(selector)?,
        None => client::known_machines(ctx)?,
    };
    let (jobs, rejected) = client::list(ctx, &machines, args.limit.unwrap_or(20));
    answer(&crate::output::Jobs {
        jobs: jobs
            .iter()
            .map(|(machine, job)| crate::output::JobView::summary(machine, job))
            .collect(),
        unreachable: rejected
            .iter()
            .map(|r| crate::output::MachineError::of(&r.machine, &r.error))
            .collect(),
    })
}

fn search_logs(ctx: &Context, args: SearchArgs) -> Result<Value, ToolError> {
    let query = client::Query {
        pattern: args.pattern,
        context: args.context.unwrap_or(2),
        limit: args.limit.unwrap_or(100),
    };
    let (machine, found) = client::search(ctx, &args.job, query)?;
    answer(&crate::output::FoundView::of(&machine.name, &found))
}

fn get_file(ctx: &Context, args: &FileArgs) -> Result<Value, ToolError> {
    let destination = inside_allowed(ctx, &args.destination)?;
    let local = |e: crate::failure::IoFailure| {
        ToolError::Destination(args.destination.clone(), e.to_string())
    };
    let mut staged = crate::user_files::Staged::beside(&destination).map_err(local)?;
    let machine = client::get(ctx, &args.job, args.path.clone(), staged.file())?;
    let bytes = staged.commit().map_err(local)?;
    answer(&crate::output::Fetched {
        machine: &machine.name,
        path: &args.path,
        written: &destination,
        bytes,
    })
}

fn call(ctx: &Context, name: &str, arguments: &Value) -> Result<Value, ToolError> {
    let tool = tool_named(name).ok_or_else(|| ToolError::Unknown(name.to_owned()))?;
    if !ctx.config.mcp.tools.contains(&tool) {
        return Err(ToolError::NotAllowed(tool_name(tool)));
    }
    match tool {
        McpTool::Run => run(ctx, parse(arguments)?),
        McpTool::ListJobs => list_jobs(ctx, &parse(arguments)?),
        McpTool::JobStatus => {
            let args: JobArgs = parse(arguments)?;
            let (machine, job) =
                client::job_request(ctx, &args.job, |job| Request::Status { job })?;
            answer(&crate::output::JobView::summary(&machine.name, &job))
        }
        McpTool::JobLogs => {
            let args: LogArgs = parse(arguments)?;
            answer(&crate::output::Log {
                log: tail(ctx, &args.job, args.lines.unwrap_or(200))?,
            })
        }
        McpTool::WaitJob => {
            let args: JobArgs = parse(arguments)?;
            let (machine, job) = client::wait(ctx, &args.job)?;
            let (machine, digest) = client::digest(
                ctx,
                &format!("{}:{}", machine.name, job.spec.id),
                DIGEST_TAIL,
            )?;
            answer(&crate::output::DigestView::of(&machine.name, &digest))
        }
        McpTool::JobDigest => {
            let args: DigestArgs = parse(arguments)?;
            let (machine, digest) =
                client::digest(ctx, &args.job, args.tail.unwrap_or(DIGEST_TAIL))?;
            answer(&crate::output::DigestView::of(&machine.name, &digest))
        }
        McpTool::SearchLogs => search_logs(ctx, parse(arguments)?),
        McpTool::GetFile => get_file(ctx, &parse(arguments)?),
        McpTool::KillJob => {
            let args: JobArgs = parse(arguments)?;
            let (machine, job) = client::job_request(ctx, &args.job, |job| Request::Kill { job })?;
            answer(&crate::output::JobView::summary(&machine.name, &job))
        }
        McpTool::Machines => {
            let configured = ctx.config.configured();
            let mut machines = Vec::with_capacity(configured.len());
            for machine in &configured {
                machines.push(crate::output::Configured {
                    machine: &machine.name,
                    host: &machine.host,
                    transport: &machine.transport,
                    labels: &machine.labels,
                    facts: crate::remote::cached_facts(&ctx.dirs, machine)
                        .map_err(ClientError::from)?,
                });
            }
            answer(&crate::output::Machines { machines })
        }
    }
}

fn respond(ctx: Option<&Context>, method: &str, params: &Value) -> Option<Value> {
    match method {
        "initialize" => Some(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "domyjob", "version": VERSION},
            "instructions": INSTRUCTIONS
        })),
        "ping" => Some(json!({})),
        "tools/list" => Some(json!({"tools": tools()})),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let outcome = match ctx {
                Some(ctx) => call(ctx, &name, &arguments),
                None => Err(ToolError::Unknown(
                    "(configuration failed to load)".to_owned(),
                )),
            };
            Some(match outcome {
                Ok(value) => {
                    json!({"content": [{"type": "text", "text": value.to_string()}], "structuredContent": value, "isError": false})
                }
                Err(error) => {
                    json!({"content": [{"type": "text", "text": error.to_string()}], "isError": true})
                }
            })
        }
        _ => None,
    }
}

pub fn serve() -> Result<(), McpError> {
    let ctx = match Context::load() {
        Ok(ctx) => Some(ctx),
        Err(error) => {
            eprintln!("domyjob: {error}");
            None
        }
    };
    let stdin = std::io::stdin();
    let out = std::sync::Mutex::new(std::io::stdout());
    let cancelled = std::sync::Mutex::new(std::collections::BTreeSet::<String>::new());
    std::thread::scope(|scope| -> Result<(), McpError> {
        for line in stdin.lock().lines() {
            let line = line.map_err(McpError::Input)?;
            let Ok(message) = crate::ingress::foreign_json_envelope(&line) else {
                continue;
            };
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if method == "notifications/cancelled" {
                if let (Some(request), Ok(mut set)) =
                    (message.pointer("/params/requestId"), cancelled.lock())
                {
                    set.insert(request.to_string());
                }
                continue;
            }
            let Some(id) = message.get("id").cloned() else {
                continue;
            };
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let (ctx, out, cancelled) = (ctx.as_ref(), &out, &cancelled);
            scope.spawn(move || {
                let reply = match respond(ctx, &method, &params) {
                    Some(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    None => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {method}")}})
                    }
                };
                let withdrawn = match cancelled.lock() {
                    Ok(mut set) => set.remove(&id.to_string()),
                    Err(_poisoned) => false,
                };
                if withdrawn {
                    return;
                }
                if let Ok(mut out) = out.lock() {
                    match writeln!(out, "{reply}").and_then(|()| out.flush()) {
                        Ok(()) | Err(_) => {}
                    }
                }
            });
        }
        Ok(())
    })
}

impl crate::ingress::Ingress for RunArgs {}
impl crate::ingress::Ingress for ListArgs {}
impl crate::ingress::Ingress for JobArgs {}
impl crate::ingress::Ingress for LogArgs {}
impl crate::ingress::Ingress for DigestArgs {}
impl crate::ingress::Ingress for SearchArgs {}
impl crate::ingress::Ingress for FileArgs {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_tools_and_rejects_unknown_methods() {
        let init = respond(None, "initialize", &Value::Null).unwrap();
        assert_eq!(init.pointer("/serverInfo/name"), Some(&json!("domyjob")));
        let listed = respond(None, "tools/list", &Value::Null).unwrap();
        let names: Vec<&str> = listed
            .pointer("/tools")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.get("name").unwrap().as_str().unwrap())
            .collect();
        assert!(names.contains(&"run") && names.contains(&"wait_job"));
        assert!(respond(None, "resources/list", &Value::Null).is_none());
        let failed = respond(None, "tools/call", &json!({"name": "run", "arguments": {}})).unwrap();
        assert_eq!(failed.get("isError"), Some(&json!(true)));
    }
}
