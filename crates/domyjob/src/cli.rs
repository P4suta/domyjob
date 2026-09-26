use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::client::{self, ClientError, Context, Order, Output, Sending, Submitted};
use crate::clock::Timestamp;
use crate::config::Machine;
use crate::domain::{EnvName, JobId, JobName, MachineName, RelPath};
use crate::notify::NotifyTarget;
use crate::paths::Dirs;
use crate::protocol::{Job, Phase, Request, Workspace};
use crate::template::Arg;

#[derive(Debug, Parser)]
#[command(
    name = "domyjob",
    version,
    about = "Send your work to any machine you can reach, run it there, and walk away",
    long_about = "Send your work to any machine you can reach, run it there, and walk away.\n\nThe directory is sent as it is, uncommitted edits included, and the command keeps running on the machine whether or not you stay. Any host your ssh config knows works as it is; domyjob installs itself there the first time.",
    after_long_help = AFTER_LONG_HELP
)]
struct Cli {
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = ColorWhen::Auto,
        help = "Color the output: auto follows the terminal, NO_COLOR, and CLICOLOR_FORCE"
    )]
    color: ColorWhen,
    #[arg(
        long,
        help = "With no command, print the overview of every machine as machine-readable JSON"
    )]
    json: bool,
    #[arg(
        long,
        help = "With no command, keep the overview on screen and redraw it as jobs start and finish"
    )]
    live: bool,
    #[command(subcommand)]
    command: Option<Top>,
}

const AFTER_LONG_HELP: &str = "\
MACHINES:
  A name from your configuration, any ssh destination such as user@host, a label such as gpu, a fact such as os=windows, @GROUP, or @all. Join labels and facts with +, and separate terms with commas.

IF A MACHINE OR THE NETWORK FAILS:
  Jobs keep running on the machine when your connection drops; ask again with status, wait, or logs. A machine that stops answering is reported as went silent.

EXIT STATUS:
  0  everything asked for succeeded
  1  a job failed, logs --grep found nothing, ls could not reach a machine, or doctor found a problem
  2  domyjob could not do what was asked
  run --wait on one machine exits with the job's own code.

ENVIRONMENT:
  DOMYJOB_CONFIG, DOMYJOB_STATE, DOMYJOB_CACHE  directories for configuration, jobs, and downloads
  NO_COLOR, CLICOLOR_FORCE                      turn color off or on

AGENTS:
  domyjob skill install teaches Claude Code to use domyjob; domyjob mcp serves the same commands as MCP tools; --json prints machine-readable output.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
enum Top {
    #[command(
        about = "Send this directory to machines and run a command there",
        long_about = "Send this directory to machines and run a command there.\n\nThe directory is sent as it is on disk, uncommitted changes included, minus what .gitignore, .ignore, and .domyjobignore exclude. Build output stays between runs, so later builds are incremental. The command runs in the same subdirectory you are in and keeps running after you disconnect.",
        after_help = "Examples:\n  domyjob run linux -- cargo test\n  domyjob run @all --wait -- make check\n  domyjob run windows+gpu @v1.2.0 -- cargo bench\n  domyjob run box --no-source -- uptime"
    )]
    Run(RunArgs),
    #[command(
        about = "Run a command on machines right now and show its output, the way ssh would",
        long_about = "Run a command on machines right now and show its output, the way ssh would.\n\nNothing is sent: the command runs in the machine's home directory, its output streams back, and the exit code is the command's own. It is still a job, so it keeps running if you disconnect and `domyjob ls` shows it.",
        after_help = "Examples:\n  domyjob on win -- Get-ChildItem Downloads\n  domyjob on linux -- df -h\n  domyjob on @all -- uptime"
    )]
    On(OnArgs),
    #[command(about = "Run a job named in this project's domyjob.toml")]
    Do(DoArgs),
    #[command(about = "List jobs on machines, newest first per machine")]
    Ls(LsArgs),
    #[command(about = "Print a job's output")]
    Logs(LogsArgs),
    #[command(about = "Show where a job stands")]
    Status(JobArgs),
    #[command(
        about = "Summarize a job in a few lines: its outcome, how long its log is, and how it ends",
        long_about = "Summarize a job in a few lines: its outcome, how long its log is, and how it ends.\n\nThis is the cheapest way to learn what happened: it reads the log on the machine and sends back only the count and the last lines, with terminal control sequences removed."
    )]
    Digest(DigestArgs),
    #[command(about = "Wait until jobs finish; exits non-zero if any of them failed")]
    Wait(WaitArgs),
    #[command(about = "Stop a job and every process it started, at once")]
    Kill(JobArgs),
    #[command(about = "Copy a file out of a job's workspace")]
    Get(GetArgs),
    #[command(
        about = "Bring the files a finished job changed back into this directory",
        long_about = "Bring the files a finished job changed back into this directory.\n\nOnly files the job added, altered, or removed come back, judged by the same ignore rules that decided what was sent. Nothing is written if any of those files changed here since the job was sent. Review, commit, sign, and push them here, where your keys are."
    )]
    Pull(PullArgs),
    #[command(
        about = "Free the disk space domyjob holds on machines: stale workspaces, and more on request",
        long_about = "Free the disk space domyjob holds on machines.\n\nWorkspaces that no remembered job last used go; they only give a build its head start. With --all-idle every workspace not in use goes, and with --logs the output logs of finished jobs are discarded too. Workspaces in use and every job record stay."
    )]
    Clean(CleanArgs),
    #[command(
        about = "Show how each kind of job has gone lately: its recent outcomes, success rate, and typical duration"
    )]
    History(HistoryArgs),
    #[command(about = "List, add, or remove machines")]
    Machines(MachinesArgs),
    #[command(about = "Install or update domyjob on machines")]
    Setup(SetupArgs),
    #[command(
        about = "Check this machine's setup, and whether each machine can be reached and runs a matching domyjob"
    )]
    Doctor(DoctorArgs),
    #[command(name = "self", about = "Manage this copy of domyjob")]
    Myself(SelfArgs),
    #[command(
        about = "Accept jobs from paired machines over the network, without ssh",
        long_about = "Accept jobs from paired machines over the network, without ssh.\n\nConnections are end-to-end encrypted and authenticated with keys exchanged when pairing. By default the server listens on this machine's Tailscale address, or on loopback when there is none."
    )]
    Serve(ServeArgs),
    #[command(about = "List or revoke paired machines")]
    Trust(TrustArgs),
    #[command(about = "Check or read the tamper-evident log of what peers asked for")]
    Audit(AuditArgs),
    #[command(
        about = "Pair with a machine running `domyjob serve --pair`",
        long_about = "Pair with a machine running `domyjob serve --pair`.\n\nBoth machines then show the same confirmation words; compare them, and confirm on the serving machine."
    )]
    Pair(PairArgs),
    #[command(hide = true)]
    Tunnel(TunnelArgs),
    #[command(about = "Start the jobs a repository event calls for (used by hook scripts)")]
    Trigger(crate::hook::Event),
    #[command(about = "Print a hook script that calls `domyjob trigger`")]
    Hook(HookArgs),
    #[command(about = "Serve domyjob's tools to an AI agent over MCP on stdin and stdout")]
    Mcp,
    #[command(
        about = "Print the manual page; read it with `domyjob man > domyjob.1 && man ./domyjob.1`"
    )]
    Man,
    #[command(
        about = "Print the skill that teaches an AI agent to use domyjob, or install it for Claude Code",
        after_help = "Examples:\n  domyjob skill                     print it\n  domyjob skill install             for every project, in ~/.claude/skills/domyjob\n  domyjob skill install --project . for this project only"
    )]
    Skill(SkillArgs),
    #[command(hide = true)]
    Node(NodeArgs),
    #[command(hide = true)]
    Watch(WatchArgs),
}

#[derive(Debug, Args)]
struct Common {
    #[arg(
        long,
        help = "Stay until the jobs finish, stream their output, and exit with their result"
    )]
    wait: bool,
    #[arg(
        long,
        requires = "wait",
        help = "With --wait, print a digest of each job when it finishes instead of streaming its output"
    )]
    digest: bool,
    #[arg(
        long,
        value_name = "REGEX",
        requires = "wait",
        conflicts_with = "digest",
        help = "With --wait, show only the output lines matching REGEX; the whole log stays on the machine"
    )]
    grep: Option<String>,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
    #[arg(
        long = "notify",
        value_name = "NOTIFIER[:TARGET]",
        help = "Notify when the jobs finish, for example desktop or ntfy:my-topic; repeatable"
    )]
    notify: Vec<String>,
}

#[derive(Debug, Args)]
struct OnArgs {
    #[arg(value_name = "MACHINES", help = "Where to run, as for `domyjob run`")]
    targets: String,
    #[arg(
        long,
        help = "The shell to run a single-word command with, instead of the machine's"
    )]
    shell: Option<String>,
    #[arg(long, help = "Print machine-readable JSON instead of the output")]
    json: bool,
    #[arg(
        last = true,
        required = true,
        value_name = "COMMAND",
        help = "The command, after --; a single word is run as a script, several as an argument list"
    )]
    input: Vec<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(
        value_name = "MACHINES",
        help = "Where to run: names, labels joined with + (windows+gpu), facts (os=linux), @group, or @all, separated by commas"
    )]
    targets: String,
    #[arg(
        value_name = "@REV",
        help = "Send a revision from version control instead of the directory as it is on disk, such as @HEAD, @main, or @v1.2.0"
    )]
    rev: Option<String>,
    #[arg(
        long,
        help = "Hand INPUT to a runner from your configuration instead of a shell, such as an AI agent"
    )]
    runner: Option<String>,
    #[arg(
        long,
        help = "Start from an empty directory that is deleted afterwards, instead of reusing the previous one"
    )]
    fresh: bool,
    #[arg(
        long = "no-source",
        help = "Send nothing and run in the machine's home directory"
    )]
    no_source: bool,
    #[arg(
        long,
        help = "The directory to send [default: the nearest one with a domyjob.toml, else the version-control root, else the current directory]"
    )]
    root: Option<PathBuf>,
    #[arg(
        long = "env",
        value_name = "KEY=VALUE",
        help = "Set an environment variable for the job; repeatable"
    )]
    env: Vec<String>,
    #[arg(
        long,
        help = "The shell that runs INPUT [default: $SHELL on Unix, pwsh or powershell on Windows]"
    )]
    shell: Option<String>,
    #[arg(
        long,
        help = "A name to refer to the job by later, as in `domyjob wait NAME`"
    )]
    name: Option<JobName>,
    #[arg(
        long,
        conflicts_with = "wait",
        help = "Show what would be sent where and what would run there, without contacting any machine"
    )]
    dry_run: bool,
    #[command(flatten)]
    common: Common,
    #[arg(
        last = true,
        required = true,
        value_name = "INPUT",
        help = "The command, after --; a single word is run as a script, several as an argument list"
    )]
    input: Vec<String>,
}

#[derive(Debug, Args)]
struct DoArgs {
    #[arg(help = "The job's name under [jobs] in domyjob.toml")]
    job: JobName,
    #[arg(
        value_name = "@REV",
        help = "Send a revision from version control instead of the directory on disk"
    )]
    rev: Option<String>,
    #[arg(
        long,
        value_name = "MACHINES",
        help = "Run on these machines instead of the job's own"
    )]
    on: Option<String>,
    #[command(flatten)]
    common: Common,
}

#[derive(Debug, Args)]
struct LsArgs {
    #[arg(help = "Which machines to ask [default: every machine domyjob knows]")]
    machines: Option<String>,
    #[arg(long, default_value_t = 20, help = "How many jobs to show per machine")]
    limit: u32,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct DigestArgs {
    #[arg(help = "A job id, a unique prefix of one, MACHINE:ID, a job name, or latest")]
    job: String,
    #[arg(
        long,
        default_value_t = 40,
        help = "How many of the last lines to include"
    )]
    tail: u32,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct LogsArgs {
    #[arg(help = "A job id, a unique prefix of one, or MACHINE:ID")]
    job: String,
    #[arg(short, long, help = "Keep printing new output until the job finishes")]
    follow: bool,
    #[arg(long, value_name = "LINES", help = "Print only the last LINES lines")]
    tail: Option<u32>,
    #[arg(
        long,
        value_name = "REGEX",
        conflicts_with_all = ["follow", "tail"],
        help = "Print only the lines matching REGEX, searched on the machine, with their line numbers"
    )]
    grep: Option<String>,
    #[arg(
        long,
        default_value_t = 2,
        requires = "grep",
        help = "Lines of context around each match"
    )]
    context: u32,
    #[arg(
        long,
        default_value_t = 200,
        requires = "grep",
        help = "Stop after this many matches"
    )]
    limit: u32,
    #[arg(long, requires = "grep", help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct JobArgs {
    #[arg(help = "A job id, a unique prefix of one, or MACHINE:ID")]
    job: String,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct WaitArgs {
    #[arg(
        required = true,
        help = "Job ids, unique prefixes of them, or MACHINE:ID"
    )]
    jobs: Vec<String>,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct GetArgs {
    #[arg(help = "A job id, a unique prefix of one, or MACHINE:ID")]
    job: String,
    #[arg(
        help = "The file, relative to the directory the job ran in; / and \\ both separate",
        value_parser = wanted_path
    )]
    path: RelPath,
    #[arg(
        short,
        long,
        help = "Write the file here instead of to standard output"
    )]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    #[arg(help = "Which machines to ask [default: every machine domyjob knows]")]
    machines: Option<String>,
    #[arg(long, help = "List jobs that ran only once too, one line each")]
    all: bool,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct FreeMore {
    #[arg(long, help = "Discard the output logs of finished jobs too")]
    logs: bool,
    #[arg(
        long,
        help = "Free every idle workspace, not only the stale ones no remembered job last used"
    )]
    all_idle: bool,
}

#[derive(Debug, Args)]
struct CleanArgs {
    #[arg(value_name = "MACHINES", help = "Which machines, as for `domyjob run`")]
    targets: String,
    #[command(flatten)]
    more: FreeMore,
    #[arg(long, help = "Show what would be freed without freeing it")]
    dry_run: bool,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct PullArgs {
    #[arg(help = "A job id, a unique prefix of one, a name, or MACHINE:ID")]
    job: String,
    #[arg(long, help = "Put back what an earlier pull of this job changed")]
    undo: bool,
    #[arg(long, help = "List the changes without writing anything")]
    dry_run: bool,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct MachinesArgs {
    #[command(subcommand)]
    action: Option<MachinesAction>,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum MachinesAction {
    #[command(
        about = "Add a machine and check that it can be reached; `domyjob setup` installs domyjob there"
    )]
    Add(AddArgs),
    #[command(about = "Forget a machine")]
    Remove(RemoveArgs),
    #[command(
        about = "Accept a machine's audit log again after you reset or reinstalled that machine yourself"
    )]
    Rewitness(RewitnessArgs),
    #[command(
        about = "Stop machines taking new jobs, for maintenance; running jobs and `on` carry on"
    )]
    Pause(PauseArgs),
    #[command(about = "Let paused machines take jobs again")]
    Resume(PauseArgs),
}

#[derive(Debug, Args)]
struct PauseArgs {
    #[arg(value_name = "MACHINES", help = "Which machines, as for `domyjob run`")]
    targets: String,
}

#[derive(Debug, Args)]
struct RewitnessArgs {
    #[arg(help = "The machine whose audit log you reset yourself")]
    name: MachineName,
    #[arg(
        long,
        help = "Trust the audit log the machine has now; without it, only show what this machine last saw"
    )]
    accept: bool,
}

#[derive(Debug, Args)]
struct AddArgs {
    #[arg(help = "The name to use for the machine, and its ssh host unless --host says otherwise")]
    name: MachineName,
    #[arg(
        long,
        help = "The host to connect to, as your ssh configuration knows it"
    )]
    host: Option<crate::domain::Host>,
    #[arg(
        long,
        help = "How to reach it: ssh, paired, local, or one from your configuration [default: ssh]"
    )]
    transport: Option<String>,
    #[arg(long = "label", help = "A label to select the machine by; repeatable")]
    labels: Vec<String>,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    #[arg(help = "The machine to forget")]
    name: MachineName,
    #[arg(
        long,
        help = "Also remove everything domyjob placed on that machine: its jobs, workspaces, key, service, and copy of domyjob"
    )]
    wipe: bool,
    #[arg(
        long,
        requires = "wipe",
        help = "With --wipe, stop jobs still running there instead of refusing"
    )]
    kill_running: bool,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(
        long,
        value_name = "loopback|tailnet|lan|ADDRESS:PORT",
        help = "Where to listen [default: tailnet when this machine has a Tailscale address, else loopback]"
    )]
    expose: Option<String>,
    #[arg(long, default_value_t = crate::serve::DEFAULT_PORT, help = "The port to listen on")]
    port: u16,
    #[arg(
        long,
        help = "Accept one new pairing, confirmed at this terminal; prints the code to type on the other machine"
    )]
    pair: bool,
    #[arg(
        long = "grant",
        value_name = "submit|observe|fetch|kill",
        requires = "pair",
        help = "What the paired machine may do besides observing its own jobs; repeatable or comma-separated"
    )]
    grants: Vec<String>,
    #[arg(long, help = "Serve even as root or an elevated administrator")]
    allow_root: bool,
    #[command(subcommand)]
    service: Option<ServiceAction>,
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    #[command(about = "Start `domyjob serve` with this login, and keep it running")]
    Install,
    #[command(about = "Stop starting `domyjob serve` with this login")]
    Uninstall,
}

#[derive(Debug, Args)]
struct TrustArgs {
    #[command(subcommand)]
    action: TrustAction,
}

#[derive(Debug, Subcommand)]
enum TrustAction {
    #[command(about = "List the machines you paired with, and the ones allowed to use this one")]
    Ls,
    #[command(about = "Remove a pairing, in either direction")]
    Revoke {
        #[arg(help = "A machine name or a key fingerprint")]
        who: String,
    },
}

#[derive(Debug, Args)]
struct AuditArgs {
    #[command(subcommand)]
    action: AuditAction,
}

#[derive(Debug, Subcommand)]
enum AuditAction {
    #[command(about = "Check that no entry was altered, removed, or truncated")]
    Verify,
    #[command(about = "Print the latest entries")]
    Tail {
        #[arg(
            short = 'n',
            long,
            default_value_t = 20,
            help = "How many entries to print"
        )]
        lines: usize,
    },
}

#[derive(Debug, Args)]
struct PairArgs {
    #[arg(help = "The four words `domyjob serve --pair` printed")]
    code: String,
    #[arg(
        long,
        value_name = "HOST[:PORT]",
        help = "The serving machine's address [default: wait for one announcing a pairing on this network]"
    )]
    at: Option<String>,
    #[arg(
        long,
        help = "The name to give the machine here [default: the name it announces]"
    )]
    name: Option<MachineName>,
}

#[derive(Debug, Args)]
struct TunnelArgs {
    machine: MachineName,
}

#[derive(Debug, Args)]
struct SelfArgs {
    #[command(subcommand)]
    action: SelfAction,
}

#[derive(Debug, Subcommand)]
enum SelfAction {
    #[command(about = "Replace this copy with the newest signed release")]
    Update {
        #[arg(long, help = "Install the release even if it is older than this copy")]
        allow_downgrade: bool,
    },
    #[command(
        about = "Remove everything domyjob keeps on this machine: its service, key, jobs, workspaces, and cached binaries"
    )]
    Uninstall {
        #[arg(long, help = "Remove it; without this, only show what would go")]
        yes: bool,
        #[arg(long, help = "Stop jobs that are still running instead of refusing")]
        kill_running: bool,
    },
}

#[derive(Debug, Args)]
struct SetupArgs {
    #[arg(value_name = "MACHINES", help = "Which machines, as for `domyjob run`")]
    targets: String,
    #[arg(
        long,
        value_name = "PATH",
        requires = "insecure_unsigned",
        help = "Install this binary instead of a signed release"
    )]
    from: Option<PathBuf>,
    #[arg(
        long,
        requires = "from",
        help = "Acknowledge that the binary from --from is not signed and will run as you on those machines"
    )]
    insecure_unsigned: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(help = "Which machines to check [default: every machine domyjob knows]")]
    machines: Option<String>,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillArgs {
    #[command(subcommand)]
    action: Option<SkillAction>,
}

#[derive(Debug, Subcommand)]
enum SkillAction {
    #[command(about = "Write the skill where Claude Code finds it")]
    Install {
        #[arg(
            long,
            value_name = "DIR",
            help = "Install it for the project in DIR instead of for every project"
        )]
        project: Option<PathBuf>,
    },
}

#[derive(Debug, Args)]
struct HookArgs {
    #[arg(
        value_name = "SOURCE",
        help = "The version control system the hook is for: git"
    )]
    source: String,
}

#[derive(Debug, Args)]
struct NodeArgs {
    #[arg(long)]
    supervise: Option<JobId>,
    #[arg(long, requires = "supervise")]
    ready_event: Option<crate::domain::BlobId>,
    #[arg(long, requires = "supervise")]
    state_dir: Option<PathBuf>,
    #[arg(long, requires = "supervise")]
    home_dir: Option<PathBuf>,
    #[arg(long, hide = true, conflicts_with = "supervise")]
    reap: Option<i32>,
}

#[derive(Debug, Args)]
struct WatchArgs {
    #[arg(long = "notify")]
    notify: Vec<String>,
    #[arg(long)]
    config_defaults: bool,
    #[arg(required = true)]
    jobs: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Node(#[from] crate::node::NodeError),
    #[error("{0:?} is not KEY=VALUE with a valid variable name")]
    Env(String),
    #[error("{0:?} is not a revision; write it as @REV")]
    Rev(String),
    #[error("writing output: {0}")]
    Output(std::io::Error),
    #[error("--ready-event is how a Windows supervisor reports that it started, and only there")]
    Readiness,
    #[error("{0}")]
    Mcp(#[from] crate::mcp::McpError),
    #[error("no hook script for {0}; write one that calls `domyjob trigger`")]
    NoHook(String),
    #[error("every machine refused the job")]
    NothingSubmitted,
    #[error("{0:?} is not a capability: use submit, observe, fetch, or kill")]
    Grant(String),
    #[error(transparent)]
    Serve(#[from] crate::serve::ServeError),
    #[error(transparent)]
    Service(#[from] crate::service::ServiceError),
    #[error("{0}")]
    Declined(String),
    #[error("--grep: {0}")]
    Pattern(String),
    #[error(transparent)]
    Pull(#[from] crate::pull::PullError),
}

const FAILED_JOB: u8 = 1;
const DOMYJOB_ERROR: u8 = 2;

const fn wants_json(command: &Top) -> bool {
    match command {
        Top::Run(RunArgs { common, .. }) | Top::Do(DoArgs { common, .. }) => common.json,
        Top::On(OnArgs { json, .. })
        | Top::Ls(LsArgs { json, .. })
        | Top::Digest(DigestArgs { json, .. })
        | Top::Status(JobArgs { json, .. })
        | Top::Kill(JobArgs { json, .. })
        | Top::Wait(WaitArgs { json, .. })
        | Top::Machines(MachinesArgs { json, .. })
        | Top::Doctor(DoctorArgs { json, .. })
        | Top::Logs(LogsArgs { json, .. })
        | Top::Pull(PullArgs { json, .. })
        | Top::Clean(CleanArgs { json, .. })
        | Top::History(HistoryArgs { json, .. }) => *json,
        Top::Get(_)
        | Top::Setup(_)
        | Top::Myself(_)
        | Top::Serve(_)
        | Top::Trust(_)
        | Top::Audit(_)
        | Top::Pair(_)
        | Top::Tunnel(_)
        | Top::Trigger(_)
        | Top::Hook(_)
        | Top::Mcp
        | Top::Man
        | Top::Skill(_)
        | Top::Node(_)
        | Top::Watch(_) => false,
    }
}

const fn diagnose(error: &CliError) -> crate::diagnosis::Diagnosis {
    use crate::diagnosis::{Diagnosis, Kind};
    match error {
        CliError::Client(client) => crate::diagnosis::of_client(client),
        CliError::Env(_)
        | CliError::Rev(_)
        | CliError::Readiness
        | CliError::Grant(_)
        | CliError::NoHook(_)
        | CliError::Pattern(_)
        | CliError::Declined(_) => Diagnosis {
            kind: Kind::Usage,
            hint: None,
        },
        CliError::NothingSubmitted => Diagnosis {
            kind: Kind::Unreachable,
            hint: Some("each machine's own error is printed above it"),
        },
        CliError::Pull(error) => crate::diagnosis::of_pull(error),
        CliError::Node(_)
        | CliError::Output(_)
        | CliError::Mcp(_)
        | CliError::Serve(_)
        | CliError::Service(_) => Diagnosis {
            kind: Kind::Local,
            hint: None,
        },
    }
}

#[must_use]
pub fn main() -> ExitCode {
    let _faults_for_the_whole_run = crate::faults::from_environment();
    let cli = Cli::parse();
    anstream::ColorChoice::write_global(match cli.color {
        ColorWhen::Auto => anstream::ColorChoice::Auto,
        ColorWhen::Always => anstream::ColorChoice::Always,
        ColorWhen::Never => anstream::ColorChoice::Never,
    });
    let json = cli.command.as_ref().map_or(cli.json, wants_json);
    let outcome = match cli.command {
        Some(command) => dispatch(command),
        None if cli.live => live(cli.json),
        None => overview(cli.json),
    };
    match outcome {
        Ok(code) => code,
        Err(CliError::Output(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(error) => {
            let diagnosis = diagnose(&error);
            if json {
                let value = serde_json::json!({"schema": crate::view::JSON_SCHEMA, "error": {
                    "kind": diagnosis.kind,
                    "message": error.to_string(),
                    "hint": diagnosis.hint,
                }});
                println!("{value}");
            } else {
                crate::ui::report_error(&error.to_string(), None, diagnosis.hint);
            }
            ExitCode::from(DOMYJOB_ERROR)
        }
    }
}

fn dispatch(command: Top) -> Result<ExitCode, CliError> {
    match command {
        Top::Node(args) => node(&args),
        Top::Run(args) => run(&args),
        Top::On(args) => on(&args),
        Top::Do(args) => run_named(&args),
        Top::Ls(args) => ls(&args),
        Top::Logs(args) => logs(&args),
        Top::Status(args) => job_command(&args, |job| Request::Status { job }).map(|(job, _)| {
            if job.succeeded() || !job.is_settled() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(FAILED_JOB)
            }
        }),
        Top::Digest(args) => digest(&args),
        Top::Kill(args) => {
            job_command(&args, |job| Request::Kill { job }).map(|_| ExitCode::SUCCESS)
        }
        Top::Wait(args) => wait(&args),
        Top::Get(args) => get(&args),
        Top::Pull(args) => pull(&args),
        Top::Clean(args) => clean(&args),
        Top::History(args) => history(&args),
        Top::Machines(args) => match &args.action {
            None => machines(args.json),
            Some(MachinesAction::Add(add)) => machines_add(add),
            Some(MachinesAction::Remove(remove)) => machines_remove(remove),
            Some(MachinesAction::Rewitness(rewitness)) => machines_rewitness(rewitness),
            Some(MachinesAction::Pause(pause)) => machines_pause(pause, true),
            Some(MachinesAction::Resume(resume)) => machines_pause(resume, false),
        },
        Top::Setup(args) => setup(&args),
        Top::Doctor(args) => doctor(&args),
        Top::Myself(SelfArgs {
            action: SelfAction::Update { allow_downgrade },
        }) => self_update(allow_downgrade),
        Top::Myself(SelfArgs {
            action: SelfAction::Uninstall { yes, kill_running },
        }) => self_uninstall(Uninstalling {
            confirmed: yes,
            kill_running,
        }),
        Top::Serve(args) => serve(&args),
        Top::Trust(args) => trust(&args),
        Top::Audit(args) => audit(&args),
        Top::Pair(args) => pair(&args),
        Top::Tunnel(args) => {
            crate::serve::tunnel(&Dirs::from_env(), &args.machine)?;
            Ok(ExitCode::SUCCESS)
        }
        Top::Trigger(event) => crate::hook::trigger(&event).map_err(Into::into),
        Top::Hook(args) => hook(&args),
        Top::Mcp => Ok(crate::mcp::serve().map(|()| ExitCode::SUCCESS)?),
        Top::Man => {
            use clap::CommandFactory as _;
            clap_mangen::Man::new(Cli::command())
                .render(&mut std::io::stdout())
                .map_err(CliError::Output)?;
            Ok(ExitCode::SUCCESS)
        }
        Top::Skill(args) => skill(&args),
        Top::Watch(args) => watch(&args),
    }
}

fn node(args: &NodeArgs) -> Result<ExitCode, CliError> {
    if let Some(group) = args.reap {
        crate::proc::reap(group);
        return Ok(ExitCode::SUCCESS);
    }
    let mut dirs = Dirs::from_env();
    if let Some(id) = &args.supervise {
        if let Some(state) = &args.state_dir {
            dirs.state.clone_from(state);
        }
        if let Some(home) = &args.home_dir {
            dirs.home.clone_from(home);
        }
        crate::supervisor::supervise(dirs, id, readiness(args)?)?;
        return Ok(ExitCode::SUCCESS);
    }
    let node = crate::node::Node::open(dirs)?;
    let input = std::io::BufReader::new(std::io::stdin());
    let mut output = std::io::stdout();
    node.serve(&crate::authz::Principal::Owner, input, &mut output)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(unix)]
fn readiness(args: &NodeArgs) -> Result<crate::proc::Readiness, CliError> {
    match &args.ready_event {
        None => Ok(crate::proc::Readiness::from_parent()),
        Some(_) => Err(CliError::Readiness),
    }
}

#[cfg(windows)]
fn readiness(args: &NodeArgs) -> Result<crate::proc::Readiness, CliError> {
    match &args.ready_event {
        Some(token) => Ok(crate::proc::Readiness::from_parent(token.clone())),
        None => Err(CliError::Readiness),
    }
}

fn parse_env(pairs: &[String]) -> Result<BTreeMap<EnvName, String>, CliError> {
    pairs
        .iter()
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| CliError::Env(pair.clone()))?;
            let key = key
                .parse::<EnvName>()
                .map_err(|_invalid| CliError::Env(pair.clone()))?;
            Ok((key, value.to_owned()))
        })
        .collect()
}

fn parse_rev(rev: Option<&String>) -> Result<Option<crate::domain::Revision>, CliError> {
    match rev {
        None => Ok(None),
        Some(text) => match text
            .strip_prefix('@')
            .map(str::parse::<crate::domain::Revision>)
        {
            Some(Ok(parsed)) => Ok(Some(parsed)),
            Some(Err(_)) | None => Err(CliError::Rev(text.clone())),
        },
    }
}

fn user_words(words: &[String]) -> Vec<Arg> {
    words
        .iter()
        .map(|w| Arg::user(&crate::input::UserText::from_cli(w.clone())))
        .collect()
}

fn notify_targets(ctx: &Context, user: &[String]) -> Vec<NotifyTarget> {
    if user.is_empty() {
        ctx.config
            .defaults
            .notify
            .iter()
            .flatten()
            .map(NotifyTarget::from_config)
            .collect()
    } else {
        user.iter()
            .map(|t| NotifyTarget::from_user(&crate::input::UserText::from_cli(t.clone())))
            .collect()
    }
}

fn current_dir() -> Result<PathBuf, CliError> {
    std::env::current_dir().map_err(CliError::Output)
}

fn on(args: &OnArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let order = Order {
        queue: crate::protocol::Queue::Now,
        targets: args.targets.clone(),
        words: user_words(&args.input),
        runner: None,
        rev: None,
        sending: Sending::Nothing,
        workspace: Workspace::Warm,
        start: current_dir()?,
        root: None,
        env: BTreeMap::new(),
        shell: args.shell.clone(),
        name: None,
    };
    let common = Common {
        wait: true,
        digest: false,
        grep: None,
        json: args.json,
        notify: Vec::new(),
    };
    follow_through_as(&ctx, &order, &common, Voice::Quiet)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Voice {
    Full,
    Quiet,
}

fn run(args: &RunArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let order = Order {
        queue: crate::protocol::Queue::Slot,
        targets: args.targets.clone(),
        words: user_words(&args.input),
        runner: args.runner.clone(),
        rev: parse_rev(args.rev.as_ref())?,
        sending: if args.no_source {
            Sending::Nothing
        } else {
            Sending::Directory
        },
        workspace: if args.fresh {
            Workspace::Fresh
        } else {
            Workspace::Warm
        },
        start: current_dir()?,
        root: args.root.clone(),
        env: parse_env(&args.env)?,
        shell: args.shell.clone(),
        name: args.name.clone(),
    };
    if args.dry_run {
        return show_preview(&client::preview(&ctx, &order)?, args.common.json);
    }
    follow_through(&ctx, &order, &args.common)
}

fn run_named(args: &DoArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let start = current_dir()?;
    let root = crate::project::find_root(&start).ok_or_else(|| {
        ClientError::from(crate::project::ProjectError::NoSuchJob(args.job.clone()))
    })?;
    let project = crate::project::Project::load(&root)
        .map_err(ClientError::from)?
        .ok_or_else(|| {
            ClientError::from(crate::project::ProjectError::NoSuchJob(args.job.clone()))
        })?;
    let def = project.job(&args.job).map_err(ClientError::from)?;
    let job_dir = match &def.dir {
        Some(dir) => root.join(dir),
        None => root.clone(),
    };

    let order = Order {
        queue: crate::protocol::Queue::Slot,
        targets: args.on.clone().unwrap_or_else(|| def.on.clone()),
        words: def
            .run
            .iter()
            .map(|w| {
                Arg::user(&crate::input::UserText::from_project_job_the_user_invoked(
                    w.clone(),
                ))
            })
            .collect(),
        runner: def.runner.clone(),
        rev: parse_rev(args.rev.as_ref())?,
        sending: Sending::Directory,
        workspace: def.workspace.unwrap_or(Workspace::Warm),
        start: job_dir,
        root: Some(root),
        env: client::env_map(def.env.as_ref())?,
        shell: None,
        name: Some(args.job.clone()),
    };
    follow_through(&ctx, &order, &args.common)
}

fn follow_through(ctx: &Context, order: &Order, common: &Common) -> Result<ExitCode, CliError> {
    follow_through_as(ctx, order, common, Voice::Full)
}

fn follow_through_as(
    ctx: &Context,
    order: &Order,
    common: &Common,
    voice: Voice,
) -> Result<ExitCode, CliError> {
    let keep = match &common.grep {
        Some(pattern) => {
            Some(regex::Regex::new(pattern).map_err(|error| CliError::Pattern(error.to_string()))?)
        }
        None => None,
    };
    let board = match voice {
        Voice::Full => crate::board::Board::new(common.wait && !common.json),
        Voice::Quiet => crate::board::Board::silent(),
    };
    let (submitted, rejected) =
        client::submit(ctx, order, &|machine, stage| board.stage(machine, stage))?;
    for item in &rejected {
        board.refused(
            &item.machine,
            &item.error.to_string(),
            crate::diagnosis::of_remote(&item.error).hint,
        );
    }
    if submitted.is_empty() {
        board.clear();
        return Err(CliError::NothingSubmitted);
    }
    let notify = notify_targets(ctx, &common.notify);
    if common.wait {
        let code = finish(
            ctx,
            &submitted,
            (&notify, rejected.len()),
            (common, &board, keep.as_ref()),
        );
        board.clear();
        return code;
    }
    announce(&submitted, common.json)?;
    if !notify.is_empty() {
        spawn_watch(&submitted, &common.notify)?;
    }
    Ok(if rejected.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn reference(item: &Submitted) -> String {
    format!("{}:{}", item.machine.name, item.job.spec.id)
}

fn announce(submitted: &[Submitted], json: bool) -> Result<(), CliError> {
    if !json && crate::view::stdout_is_a_person() {
        let arrow = crate::ui::paint(
            crate::ui::Tone::Hint,
            crate::ui::symbol(crate::ui::Symbol::Hint),
        );
        let text = submitted.iter().try_fold(String::new(), |mut text, item| {
            let id = item.job.spec.id.as_str();
            let short = format!(
                "{}:{}",
                item.machine.name,
                id.get(..crate::ui::SHORT_ID).unwrap_or(id)
            );
            std::fmt::Write::write_fmt(
                &mut text,
                format_args!("  {arrow} domyjob digest {short}   domyjob logs {short} -f\n"),
            )
            .map(|()| text)
        });
        return show(text);
    }
    let mut out = std::io::stdout().lock();
    for item in submitted {
        if json {
            let value = crate::view::job_json(&item.machine.name, &item.job);
            writeln!(out, "{value}").map_err(CliError::Output)?;
        } else {
            let revision = item
                .job
                .spec
                .source()
                .map_or_else(|| "no source".to_owned(), |s| s.revision.describe());
            writeln!(
                out,
                "{}\t{}\t{}\t{revision}",
                item.job.spec.id,
                item.machine.name,
                item.job.state().as_str()
            )
            .map_err(CliError::Output)?;
        }
    }
    Ok(())
}

fn spawn_watch(submitted: &[Submitted], notify: &[String]) -> Result<(), CliError> {
    let exe = std::env::current_exe().map_err(CliError::Output)?;
    let mut args = vec![Arg::literal("watch")];
    if notify.is_empty() {
        args.push(Arg::literal("--config-defaults"));
    }
    for target in notify {
        args.push(Arg::literal("--notify"));
        args.push(Arg::user(&crate::input::UserText::from_cli(target.clone())));
    }
    for item in submitted {
        args.push(Arg::concat(&[
            Arg::word(&item.machine.name),
            Arg::literal(":"),
            Arg::word(&item.job.spec.id),
        ]));
    }
    crate::proc::spawn_detached(&crate::spawn::Invocation::new(Arg::path(&exe), args))
        .map_err(|e| CliError::Output(std::io::Error::other(e.to_string())))?;
    Ok(())
}

struct Prefixed<'a, W: Write> {
    prefix: String,
    out: &'a std::sync::Mutex<W>,
    board: &'a crate::board::Board,
    keep: Option<&'a regex::Regex>,
    pending: Vec<u8>,
}

impl<W: Write> Write for Prefixed<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let text: Vec<u8> = self.pending.drain(..=end).collect();
            if let Some(keep) = self.keep
                && !keep.is_match(&String::from_utf8_lossy(&text))
            {
                continue;
            }
            let mut line = self.prefix.clone().into_bytes();
            line.extend(text);
            let mut out = self
                .out
                .lock()
                .map_err(|_poisoned| std::io::Error::other("stdout lock poisoned"))?;
            self.board.above(|| out.write_all(&line))?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out
            .lock()
            .map_err(|_poisoned| std::io::Error::other("stdout lock poisoned"))?
            .flush()
    }
}

impl<W: Write> Prefixed<'_, W> {
    fn finish(mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            self.write_all(b"\n")?;
        }
        self.flush()
    }
}

struct Watching<'a> {
    ctx: &'a Context,
    stream: bool,
    many: bool,
    widest: usize,
    stdout: &'a std::sync::Mutex<std::io::Stdout>,
    board: &'a crate::board::Board,
    keep: Option<&'a regex::Regex>,
    destination: crate::terminal::Destination,
}

impl Watching<'_> {
    fn one(&self, item: &Submitted) -> Result<Job, ClientError> {
        let target = reference(item);
        if self.stream {
            let name = item.machine.name.as_str();
            let prefix = match (self.many, self.board.is_live()) {
                (false, _) => String::new(),
                (true, false) => format!("{name:<width$} | ", width = self.widest),
                (true, true) => format!(
                    "{}{} {} ",
                    crate::ui::machine(&item.machine.name),
                    " ".repeat(self.widest.saturating_sub(name.len())),
                    crate::ui::paint(
                        crate::ui::Tone::Dim,
                        crate::ui::symbol(crate::ui::Symbol::Gutter)
                    )
                ),
            };
            let prefixed = Prefixed {
                prefix,
                out: self.stdout,
                board: self.board,
                keep: self.keep,
                pending: Vec::new(),
            };
            let mut sink = crate::terminal::RemoteSink::new(prefixed, self.destination);
            let followed = client::logs(self.ctx, &target, Output::Follow, &mut sink)
                .map(drop)
                .and_then(|()| {
                    sink.flush()
                        .and_then(|()| sink.into_inner().finish())
                        .map_err(|source| ClientError::Io {
                            action: "writing",
                            path: PathBuf::from("stdout"),
                            source,
                        })
                });
            if let Err(error) = followed {
                eprintln!(
                    "domyjob: {}: stopped showing its output ({error}); still waiting for it to finish",
                    item.machine.name
                );
            }
        }
        client::wait(self.ctx, &target).map(|(_, job)| job)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Succeeded,
    Failed(Option<i32>),
    Unknown,
    NotRun,
}

fn exit_for(verdicts: &[Verdict]) -> u8 {
    match verdicts {
        [Verdict::Failed(Some(code))] => match u8::try_from((*code).clamp(1, 255)) {
            Ok(byte) => byte,
            Err(_out_of_range) => FAILED_JOB,
        },
        [_, ..]
            if verdicts
                .iter()
                .all(|verdict| *verdict == Verdict::Succeeded) =>
        {
            0
        }
        _ => FAILED_JOB,
    }
}

struct Settling<'a> {
    ctx: &'a Context,
    notify: &'a [NotifyTarget],
    common: &'a Common,
    board: &'a crate::board::Board,
}

fn settle(
    Settling {
        ctx,
        notify,
        common,
        board,
    }: &Settling<'_>,
    item: &Submitted,
    outcome: Result<Job, ClientError>,
) -> Result<Verdict, CliError> {
    let json = common.json;
    let job = match outcome {
        Ok(job) => job,
        Err(error) => {
            eprintln!(
                "domyjob: {}: lost track of {} ({error}); it may still be running there, and `domyjob status {}` asks again",
                item.machine.name,
                item.job.spec.id,
                reference(item)
            );
            return Ok(Verdict::Unknown);
        }
    };
    if common.digest {
        match client::digest(ctx, &reference(item), DIGEST_TAIL) {
            Ok((_, digest)) => show_digest(&item.machine.name, &digest, json)?,
            Err(error) => {
                eprintln!("domyjob: {}: {error}", item.machine.name);
                report_final(&item.machine, &job, (json, board))?;
            }
        }
    } else {
        report_final(&item.machine, &job, (json, board))?;
    }
    for target in *notify {
        if let Err(error) = crate::notify::send(&ctx.config, target, &item.machine.name, &job) {
            eprintln!("domyjob: {error}");
        }
    }
    Ok(if job.succeeded() {
        Verdict::Succeeded
    } else {
        Verdict::Failed(job.exit_code())
    })
}

fn finish(
    ctx: &Context,
    submitted: &[Submitted],
    (notify, not_run): (&[NotifyTarget], usize),
    (common, board, keep): (&Common, &crate::board::Board, Option<&regex::Regex>),
) -> Result<ExitCode, CliError> {
    let stdout = std::sync::Mutex::new(std::io::stdout());
    let watching = Watching {
        ctx,
        stream: !common.json && !common.digest,
        many: submitted.len() > 1,
        widest: submitted
            .iter()
            .map(|item| item.machine.name.as_str().len())
            .max()
            .unwrap_or(0),
        stdout: &stdout,
        board,
        keep,
        destination: crate::terminal::Destination::of_stdout(),
    };
    let mut verdicts = vec![Verdict::Unknown; submitted.len()];
    let settling = Settling {
        ctx,
        notify,
        common,
        board,
    };
    std::thread::scope(|scope| -> Result<(), CliError> {
        let (done, finished) = std::sync::mpsc::channel();
        for (index, item) in submitted.iter().enumerate() {
            let done = done.clone();
            let watching = &watching;
            scope.spawn(move || match done.send((index, watching.one(item))) {
                Ok(()) | Err(_) => {}
            });
        }
        drop(done);
        let mut waiting: Vec<bool> = vec![true; submitted.len()];
        for (index, outcome) in finished {
            let Some(item) = submitted.get(index) else {
                continue;
            };
            if let Some(slot) = waiting.get_mut(index) {
                *slot = false;
            }
            if let Some(verdict) = verdicts.get_mut(index) {
                *verdict = settle(&settling, item, outcome)?;
            }
            let still: Vec<String> = submitted
                .iter()
                .zip(&waiting)
                .filter(|(_, waiting)| **waiting)
                .map(|(other, _)| other.machine.name.to_string())
                .collect();
            if !common.json && !board.is_live() && !board.is_quiet() && !still.is_empty() {
                eprintln!("domyjob: still waiting for {}", still.join(", "));
            }
        }
        Ok(())
    })?;
    verdicts.extend(std::iter::repeat_n(Verdict::NotRun, not_run));
    Ok(ExitCode::from(exit_for(&verdicts)))
}

const DIGEST_TAIL: u32 = 40;

fn show_preview(preview: &client::Preview, json: bool) -> Result<ExitCode, CliError> {
    let mut out = std::io::stdout().lock();
    let sent = preview.sending.as_ref();
    if json {
        let value = serde_json::json!({
            "machines": preview.machines,
            "command": preview.command.display(),
            "sending": sent.map(|s| serde_json::json!({
                "root": s.root,
                "runs_in": s.subdir,
                "revision": s.revision,
                "files": s.files,
                "bytes": s.bytes,
            })),
        });
        writeln!(out, "{value}").map_err(CliError::Output)?;
        return Ok(ExitCode::SUCCESS);
    }
    let names: Vec<String> = preview.machines.iter().map(ToString::to_string).collect();
    writeln!(out, "machines  {}", names.join(", ")).map_err(CliError::Output)?;
    match sent {
        Some(s) => {
            writeln!(
                out,
                "sending   {} ({}): {} files, {} bytes",
                s.root.display(),
                s.revision,
                s.files,
                s.bytes
            )
            .map_err(CliError::Output)?;
            let place = s
                .subdir
                .as_ref()
                .map_or_else(|| ".".to_owned(), ToString::to_string);
            writeln!(out, "runs in   {place}").map_err(CliError::Output)?;
        }
        None => writeln!(out, "sending   nothing; runs in each machine's home")
            .map_err(CliError::Output)?,
    }
    writeln!(out, "command   {}", preview.command.display()).map_err(CliError::Output)?;
    Ok(ExitCode::SUCCESS)
}

fn show_digest(
    machine: &MachineName,
    digest: &crate::protocol::Digest,
    json: bool,
) -> Result<(), CliError> {
    let job = &digest.job;
    if !json && crate::view::stdout_is_a_person() {
        return show(crate::view::digest(machine, digest));
    }
    let mut out = std::io::stdout().lock();
    if json {
        let mut value = crate::view::job_json(machine, job);
        if let Some(fields) = value.as_object_mut() {
            fields.insert("lines".to_owned(), digest.lines.into());
            fields.insert("bytes".to_owned(), digest.bytes.into());
            fields.insert(
                "tail".to_owned(),
                digest
                    .tail
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        return writeln!(out, "{value}").map_err(CliError::Output);
    }
    writeln!(
        out,
        "{machine}:{}  {}",
        job.spec.id,
        crate::notify::summary(machine, job)
    )
    .map_err(CliError::Output)?;
    let shown = crate::domain::len_u64(digest.tail.len());
    writeln!(
        out,
        "log: {} lines, {} bytes{}",
        digest.lines,
        digest.bytes,
        if shown < digest.lines {
            format!("; the last {shown}:")
        } else {
            ":".to_owned()
        }
    )
    .map_err(CliError::Output)?;
    for line in &digest.tail {
        writeln!(out, "  {line}").map_err(CliError::Output)?;
    }
    Ok(())
}

fn digest(args: &DigestArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let (machine, found) = client::digest(&ctx, &args.job, args.tail)?;
    show_digest(&machine.name, &found, args.json)?;
    Ok(if found.job.succeeded() || !found.job.is_settled() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn search(args: &LogsArgs, pattern: &str) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let query = client::Query {
        pattern: pattern.to_owned(),
        context: args.context,
        limit: args.limit,
    };
    let (machine, found) = client::search(&ctx, &args.job, query)?;
    let mut out = std::io::stdout().lock();
    if args.json {
        let hits: Vec<serde_json::Value> = found
            .hits
            .iter()
            .map(|hit| serde_json::json!({"line": hit.line, "text": hit.text.to_string(), "matched": hit.matched}))
            .collect();
        let value = serde_json::json!({"schema": crate::view::JSON_SCHEMA, "machine": machine.name, "matched": found.matched, "truncated": found.truncated, "hits": hits});
        writeln!(out, "{value}").map_err(CliError::Output)?;
    } else {
        let mut previous = None;
        for hit in &found.hits {
            if previous.is_some_and(|line: u64| line.saturating_add(1) != hit.line) {
                writeln!(out, "--").map_err(CliError::Output)?;
            }
            let mark = if hit.matched { ':' } else { '-' };
            writeln!(out, "{}{mark}{}", hit.line, hit.text).map_err(CliError::Output)?;
            previous = Some(hit.line);
        }
        if found.truncated {
            eprintln!(
                "domyjob: stopped after {} of {} matches; raise --limit to see more",
                args.limit, found.matched
            );
        }
    }
    Ok(if found.matched == 0 {
        ExitCode::from(FAILED_JOB)
    } else {
        ExitCode::SUCCESS
    })
}

fn report_final(
    machine: &Machine,
    job: &Job,
    (json, board): (bool, &crate::board::Board),
) -> Result<(), CliError> {
    if json {
        let value = crate::view::job_json(&machine.name, job);
        writeln!(std::io::stdout(), "{value}").map_err(CliError::Output)
    } else {
        if board.is_quiet() && job.succeeded() {
            return Ok(());
        }
        let line = crate::view::final_line(&machine.name, job)
            .map_err(|e| CliError::Output(std::io::Error::other(e)))?;
        board.finished(&machine.name, &line);
        Ok(())
    }
}

fn age(job: &Job, now: Timestamp) -> (String, String) {
    let age = job.spec.submitted_at.until(now).to_string();
    let took = match &job.phase {
        Phase::Finished {
            started_at: Some(start),
            finished_at,
            ..
        } => start.until(*finished_at).to_string(),
        Phase::Running { started_at, .. } | Phase::Preparing { started_at } => {
            started_at.until(now).to_string()
        }
        Phase::Queued
        | Phase::Finished {
            started_at: None, ..
        } => "-".to_owned(),
    };
    (age, took)
}

fn ls(args: &LsArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = match &args.machines {
        Some(selector) => ctx.select(selector)?,
        None => client::known_machines(&ctx)?,
    };
    let (jobs, rejected) = client::list(&ctx, &machines, args.limit);
    let failed = if rejected.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    };
    if !args.json && crate::view::stdout_is_a_person() {
        let unreachable: Vec<(MachineName, String)> = rejected
            .iter()
            .map(|item| (item.machine.clone(), item.error.to_string()))
            .collect();
        show(crate::view::listing(&jobs, &unreachable))?;
        return Ok(failed);
    }
    for item in &rejected {
        eprintln!("domyjob: {}: {}", item.machine, item.error);
    }
    let now = Timestamp::observe();
    let mut out = std::io::stdout().lock();
    if !args.json {
        writeln!(
            out,
            "{:<16}  {:<12}  {:<9}  {:>4}  {:>7}  {:>7}  COMMAND",
            "ID", "MACHINE", "STATE", "EXIT", "AGE", "TOOK"
        )
        .map_err(CliError::Output)?;
    }
    for (machine, job) in &jobs {
        if args.json {
            writeln!(out, "{}", crate::view::job_json(machine, job)).map_err(CliError::Output)?;
            continue;
        }
        let (age, took) = age(job, now);
        let exit = job
            .exit_code()
            .map_or_else(|| "-".to_owned(), |c| c.to_string());
        let label = job
            .spec
            .name
            .as_ref()
            .map_or_else(|| job.spec.command.display(), ToString::to_string);
        writeln!(
            out,
            "{:<16}  {:<12}  {:<9}  {exit:>4}  {age:>7}  {took:>7}  {label}",
            job.spec.id,
            machine.as_str(),
            job.state().as_str()
        )
        .map_err(CliError::Output)?;
    }
    Ok(failed)
}

fn logs(args: &LogsArgs) -> Result<ExitCode, CliError> {
    if let Some(pattern) = &args.grep {
        return search(args, pattern);
    }
    let ctx = Context::load()?;
    let output = match (args.tail, args.follow) {
        (Some(lines), _) => Output::Tail(lines),
        (None, true) => Output::Follow,
        (None, false) => Output::Snapshot,
    };
    let framed = output == Output::Follow && crate::ui::stderr_is_live();
    if framed
        && let Ok((machine, job)) =
            client::job_request(&ctx, &args.job, |job| Request::Status { job })
    {
        let line = crate::view::final_line(&machine.name, &job)
            .map_err(|e| CliError::Output(std::io::Error::other(e)))?;
        anstream::eprintln!(
            "{} {}",
            crate::ui::paint(crate::ui::Tone::Dim, "following"),
            line.lines().next().unwrap_or_default()
        );
    }
    let mut out = crate::terminal::RemoteSink::new(
        std::io::stdout().lock(),
        crate::terminal::Destination::of_stdout(),
    );
    client::logs(&ctx, &args.job, output, &mut out)?;
    out.flush().map_err(CliError::Output)?;
    drop(out);
    if framed
        && let Ok((machine, job)) =
            client::job_request(&ctx, &args.job, |job| Request::Status { job })
    {
        let board = crate::board::Board::new(false);
        let line = crate::view::final_line(&machine.name, &job)
            .map_err(|e| CliError::Output(std::io::Error::other(e)))?;
        board.finished(&machine.name, &line);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_job(machine: &MachineName, job: &Job, json: bool) -> Result<(), CliError> {
    if !json && crate::view::stdout_is_a_person() {
        return show(crate::view::job_detail(machine, job));
    }
    let mut out = std::io::stdout().lock();
    if json {
        writeln!(out, "{}", crate::view::job_json(machine, job))
    } else {
        writeln!(
            out,
            "{}\t{machine}\t{}",
            job.spec.id,
            crate::notify::summary(machine, job)
        )
    }
    .map_err(CliError::Output)
}

fn job_command(
    args: &JobArgs,
    request: impl Fn(crate::domain::JobRef) -> Request,
) -> Result<(Job, Machine), CliError> {
    let ctx = Context::load()?;
    let (machine, job) = client::job_request(&ctx, &args.job, request)?;
    print_job(&machine.name, &job, args.json)?;
    Ok((job, machine))
}

fn machines_pause(args: &PauseArgs, paused: bool) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = ctx.select(&args.targets)?;
    let mut all_ok = true;
    for machine in &machines {
        match client::pause(&ctx, machine, paused) {
            Ok(_) => eprintln!(
                "domyjob: {}: {}",
                machine.name,
                if paused {
                    "paused; running jobs carry on, new ones are refused until `domyjob machines resume`"
                } else {
                    "takes jobs again"
                }
            ),
            Err(error) => {
                all_ok = false;
                crate::ui::report_error(
                    &error.to_string(),
                    None,
                    crate::diagnosis::of_remote(&error).hint,
                );
            }
        }
    }
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn history(args: &HistoryArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = match &args.machines {
        Some(selector) => ctx.select(selector)?,
        None => client::known_machines(&ctx)?,
    };
    let (jobs, rejected) = client::list(&ctx, &machines, 500);
    for item in &rejected {
        crate::ui::report_error(
            &item.error.to_string(),
            None,
            crate::diagnosis::of_remote(&item.error).hint,
        );
    }
    let series = crate::history::series(&jobs);
    if args.json {
        let values: Vec<serde_json::Value> = series
            .iter()
            .map(|one| {
                serde_json::json!({
                    "machine": one.machine,
                    "name": one.label,
                    "runs": one.runs,
                    "succeeded": one.succeeded,
                    "recent": one.recent.iter().map(|state| state.as_str()).collect::<Vec<_>>(),
                    "typical_millis": one.typical.map(crate::clock::Elapsed::millis),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"schema": crate::view::JSON_SCHEMA, "history": values})
        );
    } else {
        show(crate::view::history(
            &series,
            (Timestamp::observe(), args.all),
        ))?;
    }
    Ok(if rejected.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn clean(args: &CleanArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = ctx.select(&args.targets)?;
    let mut all_ok = true;
    let mut values = Vec::new();
    std::thread::scope(|scope| -> Result<(), CliError> {
        let handles: Vec<_> = machines
            .iter()
            .map(|machine| {
                let ctx = &ctx;
                (
                    machine,
                    scope.spawn(move || {
                        client::clean(
                            ctx,
                            machine,
                            (!args.dry_run, args.more.logs, args.more.all_idle),
                        )
                    }),
                )
            })
            .collect();
        for (machine, handle) in handles {
            let result = match handle.join() {
                Ok(result) => result,
                Err(_panicked) => return Err(ClientError::Panicked.into()),
            };
            match result {
                Ok(cleaned) if args.json => values.push(serde_json::json!({
                    "machine": machine.name,
                    "cleaned": cleaned,
                })),
                Ok(cleaned) => show(crate::view::cleaned(&machine.name, &cleaned))?,
                Err(error) => {
                    all_ok = false;
                    crate::ui::report_error(
                        &error.to_string(),
                        None,
                        crate::diagnosis::of_remote(&error).hint,
                    );
                }
            }
        }
        Ok(())
    })?;
    if args.json {
        println!(
            "{}",
            serde_json::json!({"schema": crate::view::JSON_SCHEMA, "machines": values})
        );
    }
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn overview(json: bool) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = ctx.config.configured();
    if machines.is_empty() {
        show(crate::view::first_run())?;
        return Ok(ExitCode::SUCCESS);
    }
    let now = Timestamp::observe();
    let (answers, answered) = std::sync::mpsc::channel();
    let mut reachable = true;
    let mut values = Vec::new();
    std::thread::scope(|scope| -> Result<(), CliError> {
        for machine in &machines {
            let answers = answers.clone();
            let ctx = &ctx;
            scope.spawn(move || {
                let survey = client::survey(ctx, machine);
                match answers.send((machine.name.clone(), survey)) {
                    Ok(()) | Err(_) => {}
                }
            });
        }
        drop(answers);
        for (name, survey) in answered {
            match survey {
                Ok((report, jobs)) => {
                    if json {
                        values.push(overview_json(&name, &report, &jobs));
                    } else {
                        show(crate::view::machine_card(&name, &report, &jobs, now))?;
                    }
                }
                Err(error) => {
                    reachable = false;
                    if json {
                        values.push(serde_json::json!({
                            "machine": name,
                            "reachable": false,
                            "error": {
                                "message": error.to_string(),
                                "hint": crate::diagnosis::of_remote(&error).hint,
                            },
                        }));
                    } else {
                        let why = error.to_string();
                        let why = why
                            .strip_prefix(&format!("{name}: "))
                            .unwrap_or(&why)
                            .to_owned();
                        show(crate::view::unreachable_card(
                            &name,
                            &why,
                            crate::diagnosis::of_remote(&error).hint,
                        ))?;
                    }
                }
            }
        }
        Ok(())
    })?;
    if json {
        values.sort_by(|a, b| {
            a.get("machine")
                .map(ToString::to_string)
                .cmp(&b.get("machine").map(ToString::to_string))
        });
        println!(
            "{}",
            serde_json::json!({"schema": crate::view::JSON_SCHEMA, "machines": values})
        );
    }
    Ok(if reachable {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn live(json: bool) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = ctx.config.configured();
    if machines.is_empty() {
        show(crate::view::first_run())?;
        return Ok(ExitCode::SUCCESS);
    }
    let redraw = !json && crate::view::stdout_is_a_person();
    let (updates, arrivals) = std::sync::mpsc::channel();
    std::thread::scope(|scope| -> Result<(), CliError> {
        for machine in &machines {
            let updates = updates.clone();
            let ctx = &ctx;
            scope.spawn(move || {
                let name = machine.name.clone();
                let mut each = |survey: crate::protocol::Survey| match updates
                    .send((name.clone(), Ok(survey)))
                {
                    Ok(()) | Err(_) => {}
                };
                let ended = client::watch(ctx, machine, &mut each);
                let why = match ended {
                    Ok(()) => "stopped reporting".to_owned(),
                    Err(error) => error.to_string(),
                };
                match updates.send((name, Err(why))) {
                    Ok(()) | Err(_) => {}
                }
            });
        }
        drop(updates);
        let mut cards: BTreeMap<MachineName, String> = BTreeMap::new();
        let mut drawn = 0usize;
        for (name, update) in arrivals {
            if json {
                let value = match &update {
                    Ok(survey) => overview_json(&name, &survey.report, &survey.jobs),
                    Err(why) => serde_json::json!({
                        "machine": name,
                        "reachable": false,
                        "error": {"message": why},
                    }),
                };
                println!("{value}");
                continue;
            }
            let card = match update {
                Ok(survey) => crate::view::machine_card(
                    &name,
                    &survey.report,
                    &survey.jobs,
                    Timestamp::observe(),
                ),
                Err(why) => crate::view::unreachable_card(&name, &why, None),
            }
            .map_err(|e| CliError::Output(std::io::Error::other(e)))?;
            cards.insert(name, card);
            let mut screen: String = cards.values().map(String::as_str).collect();
            screen.push_str(&crate::ui::paint(
                crate::ui::Tone::Dim,
                "live: redrawn as jobs start and finish · Ctrl-C to stop\n",
            ));
            let mut out = anstream::stdout();
            if redraw && drawn > 0 {
                write!(out, "\x1b[{drawn}F\x1b[J").map_err(CliError::Output)?;
            }
            out.write_all(screen.as_bytes()).map_err(CliError::Output)?;
            out.flush().map_err(CliError::Output)?;
            drawn = screen.lines().count();
        }
        Ok(())
    })?;
    Ok(ExitCode::from(FAILED_JOB))
}

fn overview_json(
    name: &MachineName,
    report: &crate::protocol::Report,
    jobs: &[Job],
) -> serde_json::Value {
    let summaries = |wanted: &dyn Fn(&Job) -> bool| -> Vec<serde_json::Value> {
        jobs.iter()
            .filter(|job| wanted(job))
            .map(|job| crate::view::job_summary_json(name, job))
            .collect()
    };
    serde_json::json!({
        "machine": name,
        "reachable": true,
        "report": report,
        "running": summaries(&|job| matches!(job.state(), crate::protocol::State::Running | crate::protocol::State::Preparing)),
        "queued": summaries(&|job| job.state() == crate::protocol::State::Queued),
        "recent": jobs
            .iter()
            .filter(|job| job.is_settled())
            .take(5)
            .map(|job| crate::view::job_summary_json(name, job))
            .collect::<Vec<_>>(),
    })
}

fn show(text: Result<String, std::fmt::Error>) -> Result<(), CliError> {
    let text = text.map_err(|e| CliError::Output(std::io::Error::other(e)))?;
    anstream::stdout()
        .write_all(text.as_bytes())
        .map_err(CliError::Output)
}

fn wait(args: &WaitArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let mut all_ok = true;
    std::thread::scope(|scope| -> Result<(), CliError> {
        let (done, finished) = std::sync::mpsc::channel();
        for reference in &args.jobs {
            let (ctx, done) = (&ctx, done.clone());
            scope.spawn(
                move || match done.send((reference, client::wait(ctx, reference))) {
                    Ok(()) | Err(_) => {}
                },
            );
        }
        drop(done);
        for (reference, outcome) in finished {
            match outcome {
                Ok((machine, job)) => {
                    print_job(&machine.name, &job, args.json)?;
                    all_ok &= job.succeeded();
                }
                Err(error) => {
                    all_ok = false;
                    eprintln!("domyjob: {reference}: {error}");
                }
            }
        }
        Ok(())
    })?;
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn wanted_path(text: &str) -> Result<RelPath, crate::domain::Invalid> {
    text.replace('\\', "/").parse()
}

fn get(args: &GetArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    match &args.output {
        Some(path) => {
            let local = |e: crate::user_files::UserFileError| {
                CliError::Output(std::io::Error::other(e.to_string()))
            };
            let mut staged = crate::user_files::Staged::beside(path).map_err(local)?;
            let machine = client::get(&ctx, &args.job, args.path.clone(), staged.file())?;
            let bytes = staged.commit().map_err(local)?;
            eprintln!(
                "domyjob: wrote {} ({bytes} bytes) from {} on {}",
                path.display(),
                args.path,
                machine.name
            );
        }
        None => {
            let mut out = crate::terminal::RemoteSink::new(
                std::io::stdout().lock(),
                crate::terminal::Destination::of_stdout(),
            );
            client::get(&ctx, &args.job, args.path.clone(), &mut out)?;
            out.flush().map_err(CliError::Output)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn pull(args: &PullArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    if args.undo {
        return pull_undo(&ctx, args);
    }
    let pulled = client::changes(&ctx, &args.job)?;
    let reference = format!("{}:{}", pulled.machine.name, pulled.job.spec.id);
    let tree = crate::pull::Tree::open(&pulled.from.root)?;
    let steps = pulled.plan.steps().to_vec();
    let checked = pulled.plan.check(&tree)?;
    let applied = if args.dry_run {
        None
    } else {
        let journal = crate::pull::Journal::open(
            &client::pulls(&ctx),
            &format!("{}-{}", pulled.machine.name, pulled.job.spec.id),
        )?;
        Some(checked.keep(&tree, &journal)?.apply(&tree, &journal)?)
    };
    show_pulled(
        &steps,
        (&reference, tree.root()),
        (applied, Pulling::Forward),
        args.json,
    )?;
    if applied.is_some_and(|applied| applied.changed > 0) {
        eprintln!("domyjob: `domyjob pull --undo {reference}` puts them back");
    }
    Ok(ExitCode::SUCCESS)
}

fn pull_undo(ctx: &Context, args: &PullArgs) -> Result<ExitCode, CliError> {
    let (machine, job) = client::recorded(ctx, &args.job)?;
    let reference = format!("{machine}:{job}");
    let journal =
        crate::pull::Journal::find(&client::pulls(ctx), &format!("{machine}-{job}"), &reference)?;
    let (tree, plan) = journal.undo()?;
    let steps = plan.steps().to_vec();
    let checked = plan.check(&tree)?;
    let applied = if args.dry_run {
        None
    } else {
        Some(checked.apply(&tree, &journal)?)
    };
    show_pulled(
        &steps,
        (&reference, tree.root()),
        (applied, Pulling::Back),
        args.json,
    )?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pulling {
    Forward,
    Back,
}

fn show_pulled(
    steps: &[crate::pull::Step],
    (reference, root): (&str, &Path),
    (applied, pulling): (Option<crate::pull::Applied>, Pulling),
    json: bool,
) -> Result<(), CliError> {
    let mut out = std::io::stdout().lock();
    if json {
        let listed: Vec<serde_json::Value> = steps
            .iter()
            .map(|step| serde_json::json!({"path": step.path, "change": step.kind().word()}))
            .collect();
        writeln!(
            out,
            "{}",
            serde_json::json!({
                "schema": crate::view::JSON_SCHEMA,
                "job": reference,
                "root": root,
                "applied": applied.is_some(),
                "changes": listed,
            })
        )
        .map_err(CliError::Output)?;
    } else {
        for step in steps {
            writeln!(out, "{} {}", step.kind().letter(), step.path).map_err(CliError::Output)?;
        }
    }
    drop(out);
    let root = root.display();
    match (applied, pulling) {
        _ if steps.is_empty() => eprintln!("domyjob: {reference} changed no files"),
        (None, _) => {}
        (Some(applied), Pulling::Forward) if applied.changed == 0 => {
            eprintln!("domyjob: {root} already matches {reference}");
        }
        (Some(applied), Pulling::Back) if applied.changed == 0 => {
            eprintln!("domyjob: {root} is already as it was before pulling {reference}");
        }
        (Some(applied), Pulling::Forward) => eprintln!(
            "domyjob: changed {} files in {root} to match {reference}",
            applied.changed
        ),
        (Some(applied), Pulling::Back) => eprintln!(
            "domyjob: put back {} files in {root} as they were before pulling {reference}",
            applied.changed
        ),
    }
    Ok(())
}

fn machines_add(args: &AddArgs) -> Result<ExitCode, CliError> {
    let dirs = Dirs::from_env();
    let path = crate::config::path(&dirs);
    let machine = crate::config::NewMachine {
        name: args.name.clone(),
        host: args.host.clone(),
        transport: args.transport.clone(),
        labels: args.labels.clone(),
    };
    crate::config::add_machine(&path, &machine).map_err(ClientError::from)?;
    let ctx = Context::load()?;
    let configured = ctx.config.machine(&args.name);
    match crate::remote::Link::open(&ctx.config, &ctx.dirs, &configured) {
        Ok(_) => {
            let facts =
                crate::remote::cached_facts(&ctx.dirs, &configured).map_err(ClientError::from)?;
            let seen = facts.map_or_else(String::new, |f| {
                format!(
                    "{}/{}  domyjob {}  {:?}",
                    f.hello.os, f.hello.arch, f.hello.version, f.placement
                )
            });
            println!("{}  {seen}  {}  ready", args.name, configured.transport);
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            eprintln!(
                "domyjob: {} was added to {} but is not reachable yet: {error}",
                args.name,
                path.display()
            );
            Ok(ExitCode::from(DOMYJOB_ERROR))
        }
    }
}

fn machines_remove(args: &RemoveArgs) -> Result<ExitCode, CliError> {
    let dirs = Dirs::from_env();
    if args.wipe {
        wipe(&args.name, args.kill_running)?;
    }
    crate::remote::forget_witness(&dirs, &args.name).map_err(ClientError::from)?;
    match crate::config::remove_machine(&crate::config::path(&dirs), &args.name) {
        Ok(()) => {}
        Err(crate::config::ConfigError::NotConfigured(_)) if args.wipe => {}
        Err(other) => return Err(ClientError::from(other).into()),
    }
    Ok(ExitCode::SUCCESS)
}

fn wipe(name: &MachineName, kill_running: bool) -> Result<(), CliError> {
    let ctx = Context::load()?;
    let machine = ctx.config.machine(name);
    let (jobs, rejected) = client::list(&ctx, std::slice::from_ref(&machine), u32::MAX);
    if let Some(item) = rejected.into_iter().next() {
        return Err(ClientError::from(item.error).into());
    }
    let running: Vec<&Job> = jobs
        .iter()
        .map(|(_, job)| job)
        .filter(|job| !job.is_settled())
        .collect();
    if !running.is_empty() && !kill_running {
        let ids: Vec<String> = running
            .iter()
            .map(|job| format!("{name}:{}", job.spec.id))
            .collect();
        return Err(CliError::Declined(format!(
            "{name} still runs {}; wait for them, or add --kill-running to stop them first",
            ids.join(", ")
        )));
    }
    for still in running {
        let reference = format!("{name}:{}", still.spec.id);
        client::job_request(&ctx, &reference, |job| Request::Kill { job })?;
    }
    let link =
        crate::remote::Link::open(&ctx.config, &ctx.dirs, &machine).map_err(ClientError::from)?;
    let report = link.wipe().map_err(ClientError::from)?;
    if !report.is_empty() {
        eprintln!("{report}");
    }
    eprintln!("domyjob: {name}: removed everything domyjob kept there");
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Uninstalling {
    confirmed: bool,
    kill_running: bool,
}

fn self_uninstall(
    Uninstalling {
        confirmed,
        kill_running,
    }: Uninstalling,
) -> Result<ExitCode, CliError> {
    let dirs = Dirs::from_env();
    if !confirmed {
        println!(
            "would remove the domyjob service if one is installed, this machine's domyjob key, {} (jobs, workspaces, audit log), and {} (cached binaries)",
            dirs.state.display(),
            dirs.cache.display()
        );
        println!("run `domyjob self uninstall --yes` to remove them");
        return Ok(ExitCode::SUCCESS);
    }
    let node = crate::node::Node::open(dirs.clone())?;
    let running = node.running();
    if !running.is_empty() && !kill_running {
        let ids: Vec<String> = running.iter().map(ToString::to_string).collect();
        return Err(CliError::Declined(format!(
            "jobs still run here: {}; wait for them, or add --kill-running",
            ids.join(", ")
        )));
    }
    for id in running {
        node.stop(&id)?;
    }
    drop(node);
    match crate::service::uninstall(&dirs) {
        Ok(()) | Err(_) => {}
    }
    dirs.keys
        .forget(&dirs.state, "identity")
        .map_err(|e| CliError::Declined(e.to_string()))?;
    let mut kept = Vec::new();
    for dir in [&dirs.state, &dirs.cache] {
        if let Err(error) = crate::state_file::remove_tree_forcibly(dir) {
            kept.push(format!("{} ({error})", dir.display()));
        }
    }
    if kept.is_empty() {
        println!("removed domyjob's service, key, state, and cache from this machine");
    } else {
        println!(
            "removed domyjob's service, key, and what could go; still here: {}",
            kept.join(", ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn machines_rewitness(args: &RewitnessArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let known = crate::remote::witnessed(&ctx.dirs, &args.name);
    match &known {
        Some(head) => eprintln!(
            "domyjob: this machine last saw {} entries in {}'s audit log, ending in {}",
            head.seq, args.name, head.hash
        ),
        None => eprintln!(
            "domyjob: this machine has no record of {}'s audit log",
            args.name
        ),
    }
    if !args.accept {
        eprintln!(
            "domyjob: if you reset or reinstalled {} yourself, `domyjob machines rewitness {} --accept` trusts the log it has now",
            args.name, args.name
        );
        return Ok(ExitCode::SUCCESS);
    }
    crate::remote::forget_witness(&ctx.dirs, &args.name).map_err(ClientError::from)?;
    let machine = ctx.config.machine(&args.name);
    crate::remote::Link::open(&ctx.config, &ctx.dirs, &machine).map_err(ClientError::from)?;
    match crate::remote::witnessed(&ctx.dirs, &args.name) {
        Some(head) => eprintln!(
            "domyjob: now trusting {}'s audit log as it stands: {} entries, ending in {}",
            args.name, head.seq, head.hash
        ),
        None => eprintln!("domyjob: {} answered, but reported no audit log", args.name),
    }
    Ok(ExitCode::SUCCESS)
}

fn serve(args: &ServeArgs) -> Result<ExitCode, CliError> {
    let dirs = Dirs::from_env();
    if matches!(args.service, Some(ServiceAction::Install)) {
        let exe = std::env::current_exe().map_err(CliError::Output)?;
        let expose = args.expose.clone().unwrap_or_else(|| "tailnet".to_owned());
        crate::serve::Exposure::parse(&expose).map_err(ClientError::from)?;
        let serve_args = vec![
            Arg::literal("--expose"),
            Arg::user(&crate::input::UserText::from_cli(expose)),
            Arg::literal("--port"),
            Arg::number(u64::from(args.port)),
        ];
        let placed = crate::service::install(&dirs, &exe, &serve_args)?;
        println!("domyjob serve now starts with your session ({placed})");
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(args.service, Some(ServiceAction::Uninstall)) {
        crate::service::uninstall(&dirs)?;
        println!("domyjob serve no longer starts with your session");
        return Ok(ExitCode::SUCCESS);
    }
    let exposure = match &args.expose {
        Some(text) => crate::serve::Exposure::parse(text).map_err(ClientError::from)?,
        None => crate::serve::Exposure::default_for_this_machine(),
    };
    let mut capabilities = std::collections::BTreeSet::from([crate::authz::Capability::Observe]);
    for grant in args.grants.iter().flat_map(|g| g.split(',')) {
        let capability = crate::authz::Capability::parse(grant.trim())
            .ok_or_else(|| CliError::Grant(grant.to_owned()))?;
        capabilities.insert(capability);
    }
    let options = crate::serve::Options {
        exposure,
        port: args.port,
        pairing: args.pair.then_some(capabilities),
        allow_root: args.allow_root,
    };
    crate::serve::serve(dirs, &options)?;
    Ok(ExitCode::SUCCESS)
}

fn trust(args: &TrustArgs) -> Result<ExitCode, CliError> {
    let dirs = Dirs::from_env();
    match &args.action {
        TrustAction::Ls => {
            let listing = crate::serve::listing(&dirs)?;
            println!(
                "this machine's key is kept in {}",
                crate::trust::Identity::protection(&dirs).describe()
            );
            println!("machines this one may reach:");
            for (name, server) in &listing.servers {
                println!(
                    "  {name}\t{}\t{}",
                    server.address,
                    server.public_key.fingerprint()
                );
            }
            println!("machines that may reach this one:");
            for grant in &listing.grants {
                let caps: Vec<&str> = grant.capabilities.iter().map(|c| c.as_str()).collect();
                println!(
                    "  {}\t{}\t{}",
                    grant.label,
                    grant.public_key.fingerprint(),
                    caps.join(",")
                );
            }
        }
        TrustAction::Revoke { who } => {
            let removed = crate::serve::revoke(&dirs, who)?;
            println!(
                "revoked {removed} record(s) for {}",
                crate::terminal::Display::of(who)
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn audit(args: &AuditArgs) -> Result<ExitCode, CliError> {
    let log = crate::audit::AuditLog::at(&Dirs::from_env());
    match &args.action {
        AuditAction::Verify => {
            let entries = log.verify().map_err(|e| CliError::Serve(e.into()))?;
            println!("the audit log is intact: {entries} entries");
        }
        AuditAction::Tail { lines } => {
            for entry in log.tail(*lines).map_err(|e| CliError::Serve(e.into()))? {
                let subject = entry
                    .subject
                    .as_deref()
                    .map_or_else(String::new, crate::terminal::neutralize);
                println!(
                    "{}\t{}\t{:?}\t{}\t{subject}",
                    entry.seq,
                    crate::terminal::Display::of(&entry.principal),
                    entry.verdict,
                    crate::terminal::Display::of(&entry.action)
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn pair(args: &PairArgs) -> Result<ExitCode, CliError> {
    let dirs = Dirs::from_env();
    let code = crate::secure::PairingCode::parse(&args.code).map_err(ClientError::from)?;
    let paired = crate::serve::pair(&dirs, &code, args.at.as_deref(), args.name.as_ref())?;
    let granted: Vec<&str> = paired.capabilities.iter().map(|c| c.as_str()).collect();
    println!(
        "paired with {} ({}/{}) at {}  key {}",
        paired.name,
        crate::terminal::Display::of(&paired.os),
        crate::terminal::Display::of(&paired.arch),
        paired.address,
        paired.key.fingerprint()
    );
    println!("it allows this machine to: {}", granted.join(", "));
    let path = crate::config::path(&dirs);
    let entry = crate::config::NewMachine {
        name: paired.name.clone(),
        host: None,
        transport: Some("paired".to_owned()),
        labels: Vec::new(),
    };
    match crate::config::add_machine(&path, &entry) {
        Ok(()) | Err(crate::config::ConfigError::Exists(_)) => {}
        Err(error) => return Err(ClientError::from(error).into()),
    }
    let ctx = Context::load()?;
    let machine = ctx.config.machine(&paired.name);
    crate::remote::Link::open(&ctx.config, &ctx.dirs, &machine).map_err(ClientError::from)?;
    println!("try it:  domyjob run {} -- echo hello", paired.name);
    Ok(ExitCode::SUCCESS)
}

fn self_update(allow_downgrade: bool) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let downgrade = if allow_downgrade {
        crate::dist::Downgrade::Allow
    } else {
        crate::dist::Downgrade::Refuse
    };
    match crate::dist::self_update(&ctx.config, &ctx.dirs, downgrade) {
        Ok(Some(version)) => {
            println!("domyjob is now {version}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(None) => {
            println!(
                "domyjob {} is already the newest release",
                crate::protocol::VERSION
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            eprintln!("domyjob: {error}");
            Ok(ExitCode::from(DOMYJOB_ERROR))
        }
    }
}

fn machines(json: bool) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    if !json && crate::view::stdout_is_a_person() {
        let configured = ctx.config.configured();
        let mut facts = Vec::with_capacity(configured.len());
        for machine in &configured {
            facts.push(crate::remote::cached_facts(&ctx.dirs, machine).map_err(ClientError::from)?);
        }
        let rows: Vec<crate::view::Seen<'_>> = configured
            .iter()
            .zip(&facts)
            .map(|(machine, facts)| crate::view::Seen {
                name: &machine.name,
                transport: &machine.transport,
                host: machine.host.as_str(),
                labels: &machine.labels,
                facts: facts.as_ref(),
            })
            .collect();
        show(crate::view::machines(&rows))?;
        return Ok(if configured.is_empty() {
            ExitCode::from(FAILED_JOB)
        } else {
            ExitCode::SUCCESS
        });
    }
    let mut out = std::io::stdout().lock();
    for machine in ctx.config.configured() {
        let facts = crate::remote::cached_facts(&ctx.dirs, &machine).map_err(ClientError::from)?;
        if json {
            let value = serde_json::json!({
                "name": machine.name, "host": machine.host, "transport": machine.transport,
                "labels": machine.labels, "facts": facts,
            });
            writeln!(out, "{value}").map_err(CliError::Output)?;
        } else {
            let seen = facts.map_or_else(
                || "not contacted yet".to_owned(),
                |f| format!("{}/{} {}", f.hello.os, f.hello.arch, f.hello.version),
            );
            writeln!(
                out,
                "{}\t{}\t{}\t{}\t{seen}",
                machine.name,
                machine.transport,
                machine.host,
                machine.labels.join(",")
            )
            .map_err(CliError::Output)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn setup(args: &SetupArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = ctx.select(&args.targets)?;
    let install = |machine: &Machine| -> Result<crate::protocol::Hello, ClientError> {
        let chosen = match (&args.from, args.insecure_unsigned) {
            (Some(path), true) => Some(
                crate::dist::unsigned(
                    path,
                    crate::dist::InsecureUnsigned::acknowledged_on_the_command_line(),
                )
                .map_err(|e| ClientError::from(crate::remote::RemoteError::from(e)))?,
            ),
            (Some(_) | None, false) | (None, true) => None,
        };
        Ok(
            crate::remote::Link::provision(&ctx.config, &ctx.dirs, machine, chosen)
                .map(|(_, hello)| hello)?,
        )
    };
    let mut all_ok = true;
    std::thread::scope(|scope| {
        let (done, finished) = std::sync::mpsc::channel();
        for machine in &machines {
            let (install, done) = (&install, done.clone());
            scope.spawn(move || match done.send((machine, install(machine))) {
                Ok(()) | Err(_) => {}
            });
        }
        drop(done);
        for (machine, outcome) in finished {
            match outcome {
                Ok(hello) => println!(
                    "{}\t{}/{}\t{}",
                    machine.name, hello.os, hello.arch, hello.version
                ),
                Err(error) => {
                    all_ok = false;
                    eprintln!("domyjob: {}: {error}", machine.name);
                }
            }
        }
    });
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(DOMYJOB_ERROR)
    })
}

const SKILL: &str = include_str!("skill.md");

const JOB_ENVIRONMENT: &str = "jobs keep the environment of the session that started them, but not what lived only in it: a forwarded ssh agent is gone once the session closes, so clone private repositories with credentials the machine holds";

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Checked {
    Reached {
        machine: MachineName,
        os: String,
        arch: String,
        version: String,
        shell: String,
        placement: crate::remote::Placement,
    },
    Failed {
        machine: MachineName,
        kind: crate::diagnosis::Kind,
        error: String,
        hint: Option<&'static str>,
    },
}

fn checked(
    name: &MachineName,
    result: &Result<crate::remote::Facts, crate::remote::RemoteError>,
) -> Checked {
    match result {
        Ok(facts) => Checked::Reached {
            machine: name.clone(),
            os: facts.hello.os.to_string(),
            arch: facts.hello.arch.to_string(),
            version: facts.hello.version.to_string(),
            shell: facts.hello.shell.to_string(),
            placement: facts.placement,
        },
        Err(error) => {
            let diagnosis = crate::diagnosis::of_remote(error);
            Checked::Failed {
                machine: name.clone(),
                kind: diagnosis.kind,
                error: error.to_string(),
                hint: diagnosis.hint,
            }
        }
    }
}

fn print_checked(out: &mut dyn Write, row: &Checked) -> std::io::Result<()> {
    match row {
        Checked::Reached {
            machine,
            os,
            arch,
            version,
            shell,
            ..
        } => writeln!(
            out,
            "ok     {machine}: {os}/{arch} domyjob {version} shell {shell}"
        ),
        Checked::Failed {
            machine,
            error,
            hint,
            ..
        } => {
            writeln!(out, "FAIL   {machine}: {error}")?;
            match hint {
                Some(hint) => writeln!(out, "       hint: {hint}"),
                None => Ok(()),
            }
        }
    }
}

fn doctor_live(
    ctx: &Context,
    machines: &[Machine],
    (protection, fits): (&str, bool),
) -> Result<ExitCode, CliError> {
    show(crate::view::doctor_local(
        protection,
        fits,
        &ctx.dirs.state.display().to_string(),
    ))?;
    if machines.is_empty() {
        show(crate::view::first_run())?;
        return Ok(ExitCode::from(FAILED_JOB));
    }
    let widest = machines
        .iter()
        .map(|m| m.name.as_str().len())
        .max()
        .unwrap_or(0);
    let mut problems = usize::from(!fits);
    std::thread::scope(|scope| -> Result<(), CliError> {
        let (done, finished) = std::sync::mpsc::channel();
        for machine in machines {
            let done = done.clone();
            scope.spawn(
                move || match done.send((machine, client::examine_one(ctx, machine))) {
                    Ok(()) | Err(_) => {}
                },
            );
        }
        drop(done);
        for (machine, outcome) in finished {
            match outcome {
                Ok(facts) => show(crate::view::doctor_reached(&machine.name, &facts, widest))?,
                Err(error) => {
                    problems = problems.saturating_add(1);
                    let hint = crate::diagnosis::of_remote(&error).hint;
                    show(crate::view::doctor_failed(
                        &machine.name,
                        &error.to_string(),
                        hint,
                        widest,
                    ))?;
                }
            }
        }
        Ok(())
    })?;
    let verdict = match problems {
        0 => crate::ui::paint(crate::ui::Tone::Good, "everything answers"),
        1 => crate::ui::paint(crate::ui::Tone::Bad, "1 problem"),
        many => crate::ui::paint(crate::ui::Tone::Bad, &format!("{many} problems")),
    };
    show(Ok(format!("{verdict}\n")))?;
    Ok(if problems == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn doctor(args: &DoctorArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let machines = match &args.machines {
        Some(selector) => ctx.select(selector)?,
        None => ctx.config.configured(),
    };
    let protection = crate::trust::Identity::protection(&ctx.dirs).describe();
    let socket = ctx
        .dirs
        .state
        .join("v3")
        .join("live")
        .join("0000000000000000.sock");
    let socket_fits =
        socket.as_os_str().as_encoded_bytes().len() <= crate::local_socket::PATH_LIMIT;
    if !args.json && crate::view::stdout_is_a_person() {
        return doctor_live(&ctx, &machines, (protection, socket_fits));
    }
    let rows: Vec<Checked> = client::examine(&ctx, &machines)
        .iter()
        .map(|(name, result)| checked(name, result))
        .collect();
    let all_ok = socket_fits
        && rows
            .iter()
            .all(|row| matches!(row, Checked::Reached { .. }));
    let mut out = std::io::stdout().lock();
    if args.json {
        let value = serde_json::json!({
            "local": {"key_storage": protection, "state_fits_local_sockets": socket_fits},
            "machines": rows,
            "notes": [JOB_ENVIRONMENT],
        });
        writeln!(out, "{value}").map_err(CliError::Output)?;
    } else {
        writeln!(out, "this machine: its key is kept in {protection}").map_err(CliError::Output)?;
        if !socket_fits {
            writeln!(
                out,
                "this machine: {} is too long for local sockets; set DOMYJOB_STATE to a shorter directory",
                ctx.dirs.state.display()
            )
            .map_err(CliError::Output)?;
        }
        for row in &rows {
            print_checked(&mut out, row).map_err(CliError::Output)?;
        }
        writeln!(out, "note: {JOB_ENVIRONMENT}").map_err(CliError::Output)?;
    }
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(FAILED_JOB)
    })
}

fn skill(args: &SkillArgs) -> Result<ExitCode, CliError> {
    let Some(SkillAction::Install { project }) = &args.action else {
        std::io::stdout()
            .write_all(SKILL.as_bytes())
            .map_err(CliError::Output)?;
        return Ok(ExitCode::SUCCESS);
    };
    let base = match project {
        Some(dir) => dir.clone(),
        None => Dirs::from_env().home,
    };
    let path = base
        .join(".claude")
        .join("skills")
        .join("domyjob")
        .join("SKILL.md");
    crate::user_files::write(&path, SKILL.as_bytes())
        .map_err(|e| CliError::Output(std::io::Error::other(e.to_string())))?;
    println!("installed the domyjob skill at {}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn hook(args: &HookArgs) -> Result<ExitCode, CliError> {
    let script =
        crate::hook::script(&args.source).ok_or_else(|| CliError::NoHook(args.source.clone()))?;
    std::io::stdout()
        .write_all(script.as_bytes())
        .map_err(CliError::Output)?;
    Ok(ExitCode::SUCCESS)
}

fn watch(args: &WatchArgs) -> Result<ExitCode, CliError> {
    let ctx = Context::load()?;
    let targets = if args.config_defaults {
        notify_targets(&ctx, &[])
    } else {
        notify_targets(&ctx, &args.notify)
    };
    std::thread::scope(|scope| {
        for reference in &args.jobs {
            let (ctx, targets) = (&ctx, &targets);
            scope.spawn(move || watch_one(ctx, reference, targets));
        }
    });
    Ok(ExitCode::SUCCESS)
}

const WATCH_ATTEMPTS: u32 = 5;

fn watch_one(ctx: &Context, reference: &str, targets: &[NotifyTarget]) {
    let mut attempts_left = WATCH_ATTEMPTS;
    let failure = loop {
        attempts_left = attempts_left.saturating_sub(1);
        match client::wait(ctx, reference) {
            Ok((machine, job)) => {
                for target in targets {
                    if let Err(error) =
                        crate::notify::send(&ctx.config, target, &machine.name, &job)
                    {
                        note_watch(ctx, &format!("notifying about {reference}: {error}"));
                    }
                }
                return;
            }
            Err(error) if attempts_left > 0 && !matches!(error, ClientError::Unknown(_)) => {
                note_watch(
                    ctx,
                    &format!("waiting for {reference}: {error}; trying again"),
                );
            }
            Err(error) => break error,
        }
    };
    note_watch(ctx, &format!("gave up waiting for {reference}: {failure}"));
    let machine = match client::locate(ctx, reference) {
        Ok((machine, _)) => machine.name,
        Err(_unknown) => return,
    };
    for target in targets {
        let why = failure.to_string();
        let lost = crate::notify::Lost {
            machine: &machine,
            reference,
            why: &why,
        };
        if let Err(error) = crate::notify::lost(&ctx.config, target, lost) {
            note_watch(ctx, &format!("notifying about {reference}: {error}"));
        }
    }
}

fn note_watch(ctx: &Context, line: &str) {
    let path = ctx.dirs.state.join("watch.log");
    if let Ok(mut file) = crate::state_file::open_append(&path) {
        match writeln!(file, "{} {line}", Timestamp::observe()) {
            Ok(()) | Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_every_machine_running_and_succeeding_exits_zero() {
        use Verdict::{Failed, NotRun, Succeeded, Unknown};
        assert_eq!(exit_for(&[Succeeded, Succeeded]), 0);
        assert_eq!(exit_for(&[Failed(Some(3))]), 3);
        assert_eq!(exit_for(&[Failed(Some(-9))]), 1);
        for verdicts in [
            &[][..],
            &[Succeeded, NotRun],
            &[NotRun],
            &[Unknown],
            &[Succeeded, Unknown],
            &[Failed(Some(3)), Succeeded],
            &[Failed(None)],
        ] {
            assert_eq!(exit_for(verdicts), FAILED_JOB, "{verdicts:?}");
        }
    }

    #[test]
    fn a_grep_on_the_stream_shows_only_the_matching_lines() {
        let out = std::sync::Mutex::new(Vec::new());
        let board = crate::board::Board::silent();
        let keep = regex::Regex::new("^test result|panicked").unwrap();
        let mut prefixed = Prefixed {
            prefix: String::new(),
            out: &out,
            board: &board,
            keep: Some(&keep),
            pending: Vec::new(),
        };
        prefixed
            .write_all(b"   Compiling x\ntest a ... ok\nthread 'b' panicked at x\ntest res")
            .unwrap();
        prefixed.write_all(b"ult: ok. 2 passed\ntrailing").unwrap();
        prefixed.finish().unwrap();
        assert_eq!(
            String::from_utf8(out.into_inner().unwrap()).unwrap(),
            "thread 'b' panicked at x\ntest result: ok. 2 passed\n"
        );
    }

    #[test]
    fn streamed_output_keeps_its_lines_whole_however_it_is_flushed() {
        let out = std::sync::Mutex::new(Vec::new());
        let board = crate::board::Board::silent();
        let mut prefixed = Prefixed {
            prefix: "m | ".to_owned(),
            out: &out,
            board: &board,
            keep: None,
            pending: Vec::new(),
        };
        for piece in [&b"domyjob: "[..], b"here", b": ", b"full\nnext", b" half"] {
            prefixed.write_all(piece).unwrap();
            prefixed.flush().unwrap();
        }
        prefixed.finish().unwrap();
        assert_eq!(
            String::from_utf8(out.into_inner().unwrap()).unwrap(),
            "m | domyjob: here: full\nm | next half\n"
        );
    }
}
