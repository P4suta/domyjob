use std::fmt;
use std::fs;
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::NamedTempFile;

use crate::{Context as _, FAKES, Failure};

const COMMAND_LIMIT: Duration = Duration::from_secs(180);
const POLL_MILLISECONDS: u64 = 50;
pub(crate) const POLLS: u32 = 2400;

const PREFIX: &str = "domyjob-e2e-";

pub(crate) fn prepare_binaries() -> Result<tempfile::TempDir, Failure> {
    let binaries = tempfile::Builder::new()
        .prefix("domyjob-e2e-binaries-")
        .tempdir()
        .context("creating the immutable test binaries")?;
    let harness = std::env::current_exe().context("locating the harness")?;
    fs::copy(harness, binaries.path().join("harness")).context("preparing the fake binaries")?;
    fs::copy(
        env!("CARGO_BIN_EXE_domyjob"),
        binaries.path().join("domyjob"),
    )
    .context("preparing the domyjob binary under test")?;
    Ok(binaries)
}

#[derive(Debug)]
pub(crate) struct World {
    root: PathBuf,
    lock: NamedTempFile,
    machines: Vec<String>,
    background: Mutex<Vec<(String, Child)>>,
}

impl World {
    pub(crate) fn create(machines: &[&str], binaries: &Path) -> Result<Self, Failure> {
        sweep()?;
        let lock = tempfile::Builder::new()
            .prefix(PREFIX)
            .suffix(".lock")
            .tempfile()
            .context("creating the world's lock")?;
        lock.as_file().lock().context("locking the world")?;
        let root = lock.path().with_extension("");
        fs::create_dir_all(&root).context("creating the temporary root")?;
        let world = Self {
            root,
            lock,
            machines: machines.iter().map(|name| (*name).to_owned()).collect(),
            background: Mutex::new(Vec::new()),
        };
        fs::create_dir_all(world.bin()).context("creating bin")?;
        for fake in FAKES {
            fs::hard_link(binaries.join("harness"), world.program(fake))
                .context(&format!("installing the fake {fake}"))?;
        }
        fs::hard_link(binaries.join("domyjob"), world.domyjob())
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
        &self.root
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

    pub(crate) fn remove_fake(&self, name: &str) -> Result<(), Failure> {
        fs::remove_file(self.program(name)).context(&format!("removing the fake {name}"))
    }

    pub(crate) fn ssh_log(&self) -> Result<Vec<Value>, Failure> {
        crate::read_lines(&self.root().join("ssh.jsonl"))
    }

    pub(crate) fn settle(&self) -> Result<(), Failure> {
        let mut held = Vec::new();
        wait_for("every worker and AI CLI to exit", || {
            held = self.held_locks()?;
            Ok(held.is_empty().then_some(()))
        })
        .map_err(|failure| Failure::new(format!("{failure}; still locked: {held:?}")))
    }

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
                let _killed = crate::kill_process(pid);
            }
        }
        Ok(())
    }

    pub(crate) fn stop_background(&self) -> Result<(), Failure> {
        let children = std::mem::take(
            &mut *self
                .background
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for (label, mut child) in children {
            let _killed = child.kill();
            child
                .wait()
                .context(&format!("waiting for the background {label}"))?;
        }
        Ok(())
    }

    pub(crate) fn finish(self, keep: bool) -> Result<(), Failure> {
        let Self { root, lock, .. } = self;
        if keep {
            drop(lock);
            println!("    kept {}", root.display());
            return Ok(());
        }
        match fs::remove_dir_all(&root) {
            Ok(()) => Ok(()),
            Err(error) if refused(&error) => {
                println!(
                    "    left {} for a later run to remove: {error}",
                    root.display()
                );
                lock.keep().context("keeping the world's lock file")?;
                Ok(())
            }
            Err(error) => Err(Failure::new(format!(
                "removing the temporary root {}: {error}",
                root.display()
            ))),
        }
    }

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

    pub(crate) fn worker_log(&self) -> PathBuf {
        self.state().join("chat").join("worker.log")
    }

    pub(crate) fn workdir(&self, name: &str) -> Result<PathBuf, Failure> {
        let directory = self.work().join(name);
        fs::create_dir_all(&directory).context(&format!("creating {}", directory.display()))?;
        Ok(directory)
    }

    pub(crate) fn set_offline(&self, offline: bool) -> Result<(), Failure> {
        self.mark("offline", offline)
    }

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

    pub(crate) fn run(&self, arguments: &[&str]) -> Result<Run, Failure> {
        let mut command = self.command(arguments)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        execute(command, self.label(arguments))
    }

    pub(crate) fn chat(&self, arguments: &[&str]) -> Result<Run, Failure> {
        let mut full = vec!["chat", "--json"];
        full.extend_from_slice(arguments);
        self.run(&full)
    }

    pub(crate) fn chat_as(&self, agent: &str, arguments: &[&str]) -> Result<Run, Failure> {
        let mut full = vec!["chat", "--json", "--as", agent];
        full.extend_from_slice(arguments);
        self.run(&full)
    }

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

    pub(crate) fn exited(&self, code: i32) -> Result<&Self, Failure> {
        ensure!(self.code == Some(code), "expected exit code {code}: {self}");
        Ok(self)
    }

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

#[derive(Debug)]
pub(crate) struct Mcp {
    child: Child,
    input: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
    unclaimed: Vec<Value>,
    label: String,
}

impl Mcp {
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

    pub(crate) fn start(&mut self, id: u64, method: &str, params: &Value) -> Result<(), Failure> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    }

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

fn refused(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::DirectoryNotEmpty
    )
}

fn sweep() -> Result<(), Failure> {
    let temporary = std::env::temp_dir();
    for entry in fs::read_dir(&temporary).context("listing the temporary directory")? {
        let path = entry.context("listing the temporary directory")?.path();
        let is_lock = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(PREFIX))
            && path
                .extension()
                .is_some_and(|extension| extension == "lock");
        if !is_lock {
            continue;
        }
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(Failure::new(format!("opening {}: {error}", path.display())));
            }
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => continue,
            Err(fs::TryLockError::Error(error)) => {
                return Err(Failure::new(format!("locking {}: {error}", path.display())));
            }
        }
        match fs::remove_dir_all(path.with_extension("")) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if refused(&error) => continue,
            Err(error) => {
                return Err(Failure::new(format!(
                    "removing an earlier root beside {}: {error}",
                    path.display()
                )));
            }
        }
        drop(file);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound || refused(&error) => {}
            Err(error) => {
                return Err(Failure::new(format!(
                    "removing {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn wait_for<T>(
    what: &str,
    probe: impl FnMut() -> Result<Option<T>, Failure>,
) -> Result<T, Failure> {
    wait_within(what, POLLS, probe)
}

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

pub(crate) fn calls(directory: &Path) -> Result<Vec<Value>, Failure> {
    crate::read_lines(&directory.join("calls.jsonl"))
}

pub(crate) fn same_directory(left: &Path, right: &Path) -> Result<bool, Failure> {
    let left = fs::canonicalize(left).context(&format!("resolving {}", left.display()))?;
    let right = fs::canonicalize(right).context(&format!("resolving {}", right.display()))?;
    Ok(left == right)
}

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
