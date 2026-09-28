//! Temporary machines on one host, and the helpers scenarios drive them with.
//!
//! A world is one temporary root:
//!
//! - `bin/` holds the fakes and a copy of the domyjob binary under test.
//! - `machines/NAME/` holds `home/` and `work/`, and domyjob creates the private `state/` itself on first use.
//! - `ssh.jsonl` logs every connection the fake SSH saw, and `*.log` files hold the output of background processes.
//!
//! The domyjob copy runs from `bin/` because Windows looks for a program beside the running executable before it searches PATH, and that is where the fakes have to be found.

use std::fmt;
use std::fs;
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{Context as _, FAKES, Failure};

/// How long one CLI command may run before the harness stops it and fails the scenario.
const COMMAND_LIMIT: Duration = Duration::from_secs(90);
/// How often a wait polls its condition.
const POLL_MILLISECONDS: u64 = 50;
/// How many times a wait polls before it gives up by default: ten seconds in all.
const POLLS: u32 = 200;

#[derive(Debug)]
pub(crate) struct World {
    root: TempDir,
    machines: Vec<String>,
    background: Mutex<Vec<(String, Child)>>,
}

impl World {
    pub(crate) fn create(machines: &[&str]) -> Result<Self, Failure> {
        let root = tempfile::Builder::new()
            .prefix("domyjob-e2e-")
            .tempdir()
            .context("creating the temporary root")?;
        let world = Self {
            root,
            machines: machines.iter().map(|name| (*name).to_owned()).collect(),
            background: Mutex::new(Vec::new()),
        };
        fs::create_dir_all(world.bin()).context("creating bin")?;
        let harness = std::env::current_exe().context("locating the harness")?;
        for fake in FAKES {
            fs::copy(&harness, world.program(fake))
                .context(&format!("installing the fake {fake}"))?;
        }
        fs::copy(env!("CARGO_BIN_EXE_domyjob"), world.domyjob())
            .context("installing the domyjob binary under test")?;
        for name in &world.machines {
            let machine = world.machine(name);
            for directory in [machine.home(), machine.work()] {
                fs::create_dir_all(&directory)
                    .context(&format!("creating {}", directory.display()))?;
            }
        }
        Ok(world)
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.path()
    }

    fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }

    fn program(&self, name: &str) -> PathBuf {
        self.bin()
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
    }

    fn domyjob(&self) -> PathBuf {
        self.program("domyjob")
    }

    pub(crate) fn machine(&self, name: &str) -> Machine<'_> {
        Machine {
            world: self,
            name: name.to_owned(),
            dir: self.root().join("machines").join(name),
        }
    }

    /// Deletes one fake, so a scenario sees that program missing from every machine of this world.
    pub(crate) fn remove_fake(&self, name: &str) -> Result<(), Failure> {
        fs::remove_file(self.program(name)).context(&format!("removing the fake {name}"))
    }

    /// Every connection the fake SSH saw, oldest first.
    pub(crate) fn ssh_log(&self) -> Result<Vec<Value>, Failure> {
        crate::read_lines(&self.root().join("ssh.jsonl"))
    }

    /// Waits until no process holds a lock in any machine's state or work directory.
    ///
    /// domyjob holds a lock for as long as a worker or the service runs, and each fake AI CLI holds one while it runs, so this waits for the background work of every machine to end.
    /// Call it only while no client command runs: each probe takes a free lock for a moment, and a client that looks at that moment would think a worker is still running.
    pub(crate) fn settle(&self) -> Result<(), Failure> {
        let mut held = Vec::new();
        wait_for("every worker and AI CLI to exit", || {
            held = self.held_locks()?;
            Ok(held.is_empty().then_some(()))
        })
        .map_err(|failure| Failure::new(format!("{failure}; still locked: {held:?}")))
    }

    /// Stops background processes and hung AI CLIs, waits for everything else, and fails if any process of this world is left.
    pub(crate) fn cleanup(&self) -> Result<(), Failure> {
        self.stop_background()?;
        self.kill_hung()?;
        self.settle()?;
        let mut left = Vec::new();
        wait_for("every process of this world to end", || {
            left = self.processes()?;
            Ok(left.is_empty().then_some(()))
        })
        .map_err(|failure| Failure::new(format!("{failure}; still running: {left:#?}")))
    }

    /// Kills the fake AI CLIs listed in any `hang.pid`, then deletes the list so no later cleanup kills a reused PID.
    pub(crate) fn kill_hung(&self) -> Result<(), Failure> {
        let mut files = Vec::new();
        for name in &self.machines {
            files_under(
                &self.machine(name).work(),
                &|path| path.file_name().is_some_and(|file| file == "hang.pid"),
                &mut files,
            )?;
        }
        for file in files {
            let text = fs::read_to_string(&file).context(&format!("reading {}", file.display()))?;
            fs::remove_file(&file).context(&format!("removing {}", file.display()))?;
            for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
                let pid = line
                    .parse()
                    .context(&format!("reading a PID from {}", file.display()))?;
                // A process that already ended cannot be killed, and settling afterwards reports anything still running.
                let _killed = crate::kill_process(pid);
            }
        }
        Ok(())
    }

    /// Kills every background process a scenario started and waits for each to end.
    pub(crate) fn stop_background(&self) -> Result<(), Failure> {
        let children = std::mem::take(
            &mut *self
                .background
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for (label, mut child) in children {
            // A child that already ended cannot be killed; waiting still reaps it.
            let _killed = child.kill();
            child
                .wait()
                .context(&format!("waiting for the background {label}"))?;
        }
        Ok(())
    }

    /// Removes the temporary root, or keeps it and says where it is.
    pub(crate) fn finish(self, keep: bool) -> Result<(), Failure> {
        if keep {
            println!("    kept {}", self.root.keep().display());
            return Ok(());
        }
        self.root.close().context("removing the temporary root")
    }

    /// The processes whose command line mentions this world, by PID with their command lines.
    ///
    /// The temporary root's own name is random, so it identifies this world however its path is spelled.
    pub(crate) fn processes(&self) -> Result<Vec<(u32, String)>, Failure> {
        let marker = self
            .root()
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .ok_or_else(|| Failure::new("the temporary root has no name"))?;
        let listing = if cfg!(windows) {
            Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }",
                ])
                .stdin(Stdio::null())
                .output()
        } else {
            Command::new("ps")
                .args(["-A", "-ww", "-o", "pid=", "-o", "args="])
                .stdin(Stdio::null())
                .output()
        }
        .context("listing processes")?;
        ensure!(
            listing.status.success(),
            "listing processes failed: {}",
            String::from_utf8_lossy(&listing.stderr)
        );
        Ok(String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter_map(|line| {
                let (pid, command) = line.trim().split_once(' ')?;
                let pid = pid.parse().ok()?;
                command
                    .to_lowercase()
                    .contains(&marker)
                    .then(|| (pid, command.trim().to_owned()))
            })
            .collect())
    }

    fn held_locks(&self) -> Result<Vec<PathBuf>, Failure> {
        let mut locks = Vec::new();
        for name in &self.machines {
            let machine = self.machine(name);
            for directory in [machine.state(), machine.work()] {
                files_under(
                    &directory,
                    &|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "lock")
                    },
                    &mut locks,
                )?;
            }
        }
        let mut held = Vec::new();
        for lock in locks {
            let file = match fs::File::open(&lock) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Failure::new(format!("opening {}: {error}", lock.display())));
                }
            };
            match file.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => held.push(lock),
                Err(fs::TryLockError::Error(error)) => {
                    return Err(Failure::new(format!("probing {}: {error}", lock.display())));
                }
            }
        }
        Ok(held)
    }
}

/// One machine of a world, with its own state, home, and work directories.
#[derive(Debug)]
pub(crate) struct Machine<'world> {
    world: &'world World,
    name: String,
    dir: PathBuf,
}

impl Machine<'_> {
    pub(crate) fn state(&self) -> PathBuf {
        self.dir.join("state")
    }

    pub(crate) fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    pub(crate) fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    /// The log a managed turn's worker writes its errors to.
    pub(crate) fn worker_log(&self) -> PathBuf {
        self.state().join("chat").join("worker.log")
    }

    /// Creates `work/NAME`, a working directory of its own for one agent.
    pub(crate) fn workdir(&self, name: &str) -> Result<PathBuf, Failure> {
        let directory = self.work().join(name);
        fs::create_dir_all(&directory).context(&format!("creating {}", directory.display()))?;
        Ok(directory)
    }

    /// Makes the fake SSH refuse new connections to this machine and cut open ones, or accept them again.
    pub(crate) fn set_offline(&self, offline: bool) -> Result<(), Failure> {
        self.mark("offline", offline)
    }

    /// Makes the fake SSH lose every reply from this machine after its node has finished, or deliver them again.
    pub(crate) fn set_drop_reply(&self, drop_reply: bool) -> Result<(), Failure> {
        self.mark("drop-reply", drop_reply)
    }

    fn mark(&self, name: &str, present: bool) -> Result<(), Failure> {
        let path = self.dir.join(name);
        let changed = if present {
            fs::write(&path, b"")
        } else {
            match fs::remove_file(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                removed => removed,
            }
        };
        changed.context(&format!("setting {}", path.display()))
    }

    /// The domyjob client of this machine, with its state, home, PATH, and working directory.
    fn command(&self, arguments: &[&str]) -> Result<Command, Failure> {
        let world = self.world;
        let mut command = Command::new(world.domyjob());
        command
            .args(arguments)
            .current_dir(self.work())
            .env("DOMYJOB_STATE", self.state())
            .env("HOME", self.home())
            .env("PATH", crate::search_path(&world.bin())?)
            .env("E2E_ROOT", world.root())
            .env("E2E_DOMYJOB", world.domyjob())
            .env("DOMYJOB_REFRESH", "never")
            .env_remove("DOMYJOB_CHAT_AGENT")
            .env_remove("DOMYJOB_CHAT_TURN");
        if cfg!(windows) {
            command.env("USERPROFILE", self.home());
        }
        Ok(command)
    }

    fn label(&self, arguments: &[&str]) -> String {
        format!("domyjob {} on {}", arguments.join(" "), self.name)
    }

    /// Runs the domyjob client on this machine, stopping it after [`COMMAND_LIMIT`].
    pub(crate) fn run(&self, arguments: &[&str]) -> Result<Run, Failure> {
        let mut command = self.command(arguments)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        execute(command, self.label(arguments))
    }

    /// Runs `domyjob chat --json ARGUMENTS` on this machine.
    pub(crate) fn chat(&self, arguments: &[&str]) -> Result<Run, Failure> {
        let mut full = vec!["chat", "--json"];
        full.extend_from_slice(arguments);
        self.run(&full)
    }

    /// Runs `domyjob chat --json --as AGENT ARGUMENTS` on this machine.
    pub(crate) fn chat_as(&self, agent: &str, arguments: &[&str]) -> Result<Run, Failure> {
        let mut full = vec!["chat", "--json", "--as", agent];
        full.extend_from_slice(arguments);
        self.run(&full)
    }

    /// Starts the domyjob client in the background until the scenario or its cleanup stops it.
    pub(crate) fn start_background(&self, arguments: &[&str]) -> Result<u32, Failure> {
        let label = self.label(arguments);
        let log = self
            .world
            .root()
            .join(format!("{}-{}.log", self.name, arguments.join("-")));
        let output = fs::File::create(&log).context(&format!("creating {}", log.display()))?;
        let errors = output.try_clone().context("sharing the log")?;
        let mut command = self.command(arguments)?;
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(errors))
            .spawn()
            .context(&format!("starting {label}"))?;
        let pid = child.id();
        self.world
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((label, child));
        Ok(pid)
    }

    /// Starts `domyjob mcp ARGUMENTS` on this machine with its standard input and output held by the scenario.
    pub(crate) fn mcp(&self, arguments: &[&str]) -> Result<Mcp, Failure> {
        let mut full = vec!["mcp"];
        full.extend_from_slice(arguments);
        let label = self.label(&full);
        let log = self.world.root().join(format!("{}-mcp.log", self.name));
        let errors = fs::File::create(&log).context(&format!("creating {}", log.display()))?;
        let mut child = self
            .command(&full)?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(errors))
            .spawn()
            .context(&format!("starting {label}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Failure::new("the MCP server has no standard output"))?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    return;
                };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Ok(Mcp {
            input: child.stdin.take(),
            child,
            lines,
            seen: Vec::new(),
            unclaimed: Vec::new(),
            label,
        })
    }
}

fn execute(mut command: Command, label: String) -> Result<Run, Failure> {
    let child = command.spawn().context(&format!("starting {label}"))?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _delivered = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(COMMAND_LIMIT) {
        Ok(Ok(output)) => Ok(Run::new(label, &output)),
        Ok(Err(error)) => Err(Failure::new(format!("waiting for {label}: {error}"))),
        Err(_expired) => {
            let _killed = crate::kill_process(pid);
            Err(Failure::new(format!(
                "`{label}` did not finish within {} seconds; it or a process it started kept its output open",
                COMMAND_LIMIT.as_secs()
            )))
        }
    }
}

/// What one CLI command did.
#[derive(Debug)]
pub(crate) struct Run {
    label: String,
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Run {
    fn new(label: String, output: &Output) -> Self {
        Self {
            label,
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Fails unless the command exited with `code`.
    pub(crate) fn exited(&self, code: i32) -> Result<&Self, Failure> {
        ensure!(self.code == Some(code), "expected exit code {code}: {self}");
        Ok(self)
    }

    /// Parses standard output as one JSON document.
    pub(crate) fn json(&self) -> Result<Value, Failure> {
        serde_json::from_str(self.stdout.trim())
            .map_err(|error| Failure::new(format!("standard output is not JSON ({error}): {self}")))
    }
}

impl fmt::Display for Run {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "`{}` exited with {code}", self.label)?,
            None => write!(formatter, "`{}` was killed by a signal", self.label)?,
        }
        write!(
            formatter,
            "\nstdout:\n{}\nstderr:\n{}",
            self.stdout.trim_end(),
            self.stderr.trim_end()
        )
    }
}

/// A `domyjob mcp` process driven over its standard input and output.
#[derive(Debug)]
pub(crate) struct Mcp {
    child: Child,
    input: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    /// Every line the server printed, in order.
    seen: Vec<String>,
    /// Parsed messages that no caller claimed yet.
    unclaimed: Vec<Value>,
    label: String,
}

impl Mcp {
    /// Writes one JSON-RPC message as a line.
    pub(crate) fn send(&mut self, message: &Value) -> Result<(), Failure> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| Failure::new("the MCP input is closed"))?;
        writeln!(input, "{message}")
            .and_then(|()| input.flush())
            .context(&format!("writing to {}", self.label))
    }

    pub(crate) fn notify(&mut self, method: &str, params: &Value) -> Result<(), Failure> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// Sends a request without waiting for its response.
    pub(crate) fn start(&mut self, id: u64, method: &str, params: &Value) -> Result<(), Failure> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    }

    /// The response to request `id`, if it arrives within `milliseconds`.
    pub(crate) fn response(
        &mut self,
        id: u64,
        milliseconds: u64,
    ) -> Result<Option<Value>, Failure> {
        let wanted = json!(id);
        if let Some(position) = self
            .unclaimed
            .iter()
            .position(|message| message.get("id") == Some(&wanted))
        {
            return Ok(Some(self.unclaimed.remove(position)));
        }
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(milliseconds))
            .ok_or_else(|| Failure::new("the MCP wait overflows the clock"))?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Failure::new(format!(
                        "{} closed its output while waiting for response {id}",
                        self.label
                    )));
                }
            };
            self.seen.push(line.clone());
            let message: Value = serde_json::from_str(&line).context(&format!(
                "{} printed a line that is not JSON: {line:?}",
                self.label
            ))?;
            if message.get("id") == Some(&wanted) {
                return Ok(Some(message));
            }
            self.unclaimed.push(message);
        }
    }

    /// Sends a request and waits up to ten seconds for its response.
    pub(crate) fn request(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
    ) -> Result<Value, Failure> {
        self.start(id, method, params)?;
        self.response(id, 10_000)?
            .ok_or_else(|| Failure::new(format!("{} did not answer request {id}", self.label)))
    }

    /// Calls one tool and returns the call's result object.
    pub(crate) fn call(
        &mut self,
        id: u64,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value, Failure> {
        let response = self.request(
            id,
            "tools/call",
            &json!({"name": tool, "arguments": arguments}),
        )?;
        response
            .get("result")
            .cloned()
            .ok_or_else(|| Failure::new(format!("{tool} returned no result: {response}")))
    }

    /// Closes the input, waits for the server to exit, and returns every line it printed.
    pub(crate) fn finish(mut self) -> Result<Vec<String>, Failure> {
        drop(self.input.take());
        let mut exited = None;
        for _ in 0..POLLS {
            exited = self
                .child
                .try_wait()
                .context(&format!("waiting for {}", self.label))?;
            if exited.is_some() {
                break;
            }
            crate::pause(POLL_MILLISECONDS);
        }
        let Some(status) = exited else {
            let _killed = self.child.kill();
            let _reaped = self.child.wait();
            return Err(Failure::new(format!(
                "{} did not exit after its input closed",
                self.label
            )));
        };
        ensure!(status.success(), "{} exited with {status}", self.label);
        self.seen.extend(self.lines.try_iter());
        Ok(self.seen)
    }
}

/// Polls `probe` every [`POLL_MILLISECONDS`] until it yields a value, at most [`POLLS`] times.
pub(crate) fn wait_for<T>(
    what: &str,
    probe: impl FnMut() -> Result<Option<T>, Failure>,
) -> Result<T, Failure> {
    wait_within(what, POLLS, probe)
}

/// Polls `probe` every [`POLL_MILLISECONDS`] until it yields a value, at most `polls` times.
pub(crate) fn wait_within<T>(
    what: &str,
    polls: u32,
    mut probe: impl FnMut() -> Result<Option<T>, Failure>,
) -> Result<T, Failure> {
    for _ in 0..polls {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        crate::pause(POLL_MILLISECONDS);
    }
    Err(Failure::new(format!(
        "gave up waiting for {what} after {polls} polls {POLL_MILLISECONDS} ms apart"
    )))
}

/// The calls the fake AI CLIs recorded in `directory`, oldest first.
pub(crate) fn calls(directory: &Path) -> Result<Vec<Value>, Failure> {
    crate::read_lines(&directory.join("calls.jsonl"))
}

/// Whether two paths name the same existing directory, however each is spelled.
pub(crate) fn same_directory(left: &Path, right: &Path) -> Result<bool, Failure> {
    let left = fs::canonicalize(left).context(&format!("resolving {}", left.display()))?;
    let right = fs::canonicalize(right).context(&format!("resolving {}", right.display()))?;
    Ok(left == right)
}

/// The files under `directory` whose bytes contain `needle`.
pub(crate) fn files_containing(directory: &Path, needle: &str) -> Result<Vec<PathBuf>, Failure> {
    let mut files = Vec::new();
    files_under(
        directory,
        &|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()),
        &mut files,
    )?;
    let needle = needle.as_bytes();
    let mut found = Vec::new();
    for file in files {
        let bytes = match fs::read(&file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(Failure::new(format!("reading {}: {error}", file.display())));
            }
        };
        if bytes.windows(needle.len()).any(|window| window == needle) {
            found.push(file);
        }
    }
    Ok(found)
}

fn files_under(
    directory: &Path,
    wanted: &dyn Fn(&Path) -> bool,
    found: &mut Vec<PathBuf>,
) -> Result<(), Failure> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Failure::new(format!(
                "listing {}: {error}",
                directory.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.context(&format!("listing {}", directory.display()))?;
        let path = entry.path();
        if entry
            .file_type()
            .context(&format!("inspecting {}", path.display()))?
            .is_dir()
        {
            files_under(&path, wanted, found)?;
        } else if wanted(&path) {
            found.push(path);
        }
    }
    Ok(())
}
