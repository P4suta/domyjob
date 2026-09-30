#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "the harness drives real processes and files, bounds every wait, and reports on its output"
)]

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;

macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err($crate::Failure::new(format!($($message)+)));
        }
    };
}

#[path = "e2e/os.rs"]
mod os;
#[path = "e2e/provider.rs"]
mod provider;
#[path = "e2e/scenarios.rs"]
mod scenarios;
#[path = "e2e/ssh.rs"]
mod ssh;
#[path = "e2e/world.rs"]
mod world;

use world::World;

const FAKES: [&str; 5] = ["ssh", "claude", "codex", "opencode", "sleeper"];
const AI_CLIENTS: [&str; 3] = ["claude", "codex", "opencode"];

fn main() -> ExitCode {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("e2e: cannot locate this executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    match executable.file_stem().and_then(OsStr::to_str) {
        Some("ssh") => ssh::main(),
        Some("claude") => provider::main(provider::Tool::Claude),
        Some("codex") => provider::main(provider::Tool::Codex),
        Some("opencode") => provider::main(provider::Tool::Opencode),
        Some("sleeper") => provider::sleeper(),
        Some(_) | None => runner(),
    }
}

#[derive(Debug)]
struct Failure(String);

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

trait Context<T> {
    fn context(self, doing: &str) -> Result<T, Failure>;
}

impl<T, E: fmt::Display> Context<T> for Result<T, E> {
    fn context(self, doing: &str) -> Result<T, Failure> {
        self.map_err(|error| Failure(format!("{doing}: {error}")))
    }
}

struct Scenario {
    name: &'static str,
    machines: &'static [&'static str],
    run: fn(&World) -> Result<(), Failure>,
}

fn append_line(path: &Path, value: &Value) -> io::Result<usize> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let lines = match fs::read_to_string(path) {
        Ok(text) => text.lines().count(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    log.write_all(format!("{value}\n").as_bytes())?;
    drop(lock);
    Ok(lines.saturating_add(1))
}

fn read_lines(path: &Path) -> Result<Vec<Value>, Failure> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Failure::new(format!("reading {}: {error}", path.display()))),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).context(&format!("parsing a line of {}", path.display()))
        })
        .collect()
}

fn present(path: &Path) -> Result<bool, Failure> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Failure::new(format!(
            "inspecting {}: {error}",
            path.display()
        ))),
    }
}

fn holds_ai_client(directory: &Path) -> bool {
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    AI_CLIENTS.iter().any(|name| {
        suffixes
            .iter()
            .any(|suffix| fs::metadata(directory.join(format!("{name}{suffix}"))).is_ok())
    })
}

fn search_path(bin: &Path) -> Result<OsString, Failure> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let others: Vec<PathBuf> = std::env::split_paths(&inherited)
        .filter(|entry| entry != bin && !holds_ai_client(entry))
        .collect();
    std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(others)).context("joining PATH")
}

fn pause(milliseconds: u64) {
    std::thread::sleep(Duration::from_millis(milliseconds));
}

fn kill_process(pid: u32) -> io::Result<ExitStatus> {
    let pid = pid.to_string();
    let mut command = if cfg!(windows) {
        let mut command = Command::new("taskkill");
        command.args(["/F", "/PID", &pid]);
        command
    } else {
        let mut command = Command::new("kill");
        command.args(["-9", &pid]);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

fn exit_code(status: ExitStatus) -> ExitCode {
    match status.code().map(u8::try_from) {
        Some(Ok(code)) => ExitCode::from(code),
        Some(Err(_)) | None => ExitCode::from(255),
    }
}

#[derive(Debug, Default)]
struct Options {
    list: bool,
    ignored: bool,
    exact: bool,
    filters: Vec<String>,
    skips: Vec<String>,
}

impl Options {
    fn parse(mut arguments: impl Iterator<Item = String>) -> Self {
        let mut options = Self::default();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--list" => options.list = true,
                "--ignored" => options.ignored = true,
                "--exact" => options.exact = true,
                "--skip" => options.skips.extend(arguments.next()),
                "--format" | "--color" | "--test-threads" | "--logfile" | "-Z" => {
                    let _value = arguments.next();
                }
                flag if flag.starts_with('-') => {}
                filter => options.filters.push(filter.to_owned()),
            }
        }
        options
    }

    fn selects(&self, name: &str) -> bool {
        let matches = |pattern: &String| {
            if self.exact {
                name == pattern
            } else {
                name.contains(pattern.as_str())
            }
        };
        !self.ignored
            && (self.filters.is_empty() || self.filters.iter().any(matches))
            && !self.skips.iter().any(matches)
    }
}

fn jobs() -> usize {
    std::env::var("E2E_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse().ok())
        .filter(|jobs| *jobs > 0)
        .unwrap_or(4)
}

fn runner() -> ExitCode {
    let options = Options::parse(
        std::env::args_os()
            .skip(1)
            .map(|argument| argument.to_string_lossy().into_owned()),
    );
    let selected: Vec<&Scenario> = scenarios::ALL
        .iter()
        .filter(|scenario| options.selects(scenario.name))
        .collect();
    if options.list {
        for scenario in selected {
            println!("{}: test", scenario.name);
        }
        return ExitCode::SUCCESS;
    }
    let binaries = match world::prepare_binaries() {
        Ok(binaries) => binaries,
        Err(failure) => {
            eprintln!("e2e: {failure}");
            return ExitCode::FAILURE;
        }
    };
    let keep = std::env::var_os("E2E_KEEP").is_some();
    println!("\nrunning {} end-to-end scenarios", selected.len());
    let started = Instant::now();
    let queue = Mutex::new(selected.into_iter());
    let (sender, receiver) = mpsc::channel();
    let mut tally = Tally::default();
    std::thread::scope(|scope| {
        for _ in 0..jobs() {
            let (queue, sender, binaries) = (&queue, sender.clone(), binaries.path());
            scope.spawn(move || {
                loop {
                    let next = queue.lock().unwrap_or_else(PoisonError::into_inner).next();
                    let Some(scenario) = next else {
                        return;
                    };
                    let clock = Instant::now();
                    let result = attempt(scenario, binaries, keep);
                    if sender.send((scenario, result, clock.elapsed())).is_err() {
                        return;
                    }
                }
            });
        }
        drop(sender);
        for (scenario, result, took) in receiver {
            tally.record(scenario, result, took);
        }
    });
    tally.finish(started.elapsed())
}

fn attempt(scenario: &Scenario, binaries: &Path, keep: bool) -> Result<(), Failure> {
    let world = World::create(scenario.machines, binaries)?;
    let outcome = (scenario.run)(&world);
    let cleaned = world
        .cleanup()
        .map_err(|failure| Failure::new(format!("cleanup: {failure}")));
    let removed = world.finish(keep);
    let problems: Vec<String> = [outcome, cleaned, removed]
        .into_iter()
        .filter_map(Result::err)
        .map(|failure| failure.0)
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::new(problems.join("\n")))
    }
}

#[derive(Debug, Default)]
struct Tally {
    passed: usize,
    failed: usize,
}

impl Tally {
    fn record(&mut self, scenario: &Scenario, result: Result<(), Failure>, took: Duration) {
        let name = scenario.name;
        let seconds = took.as_secs_f64();
        match result {
            Ok(()) => {
                self.passed = self.passed.saturating_add(1);
                println!("e2e {name} ... ok ({seconds:.2}s)");
            }
            Err(failure) => {
                self.failed = self.failed.saturating_add(1);
                println!(
                    "e2e {name} ... FAILED ({seconds:.2}s)\n{}",
                    indent(&failure.0)
                );
            }
        }
    }

    fn finish(&self, took: Duration) -> ExitCode {
        let verdict = if self.failed == 0 { "ok" } else { "FAILED" };
        println!(
            "\ne2e result: {verdict}. {} passed; {} failed; finished in {:.2}s\n",
            self.passed,
            self.failed,
            took.as_secs_f64()
        );
        if self.failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}

fn indent(text: &str) -> String {
    let mut indented = String::new();
    for line in text.lines() {
        indented.push_str("    ");
        indented.push_str(line);
        indented.push('\n');
    }
    indented
}
