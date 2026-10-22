use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};
use tempfile::NamedTempFile;

use crate::peer::{Peer, Wait};
use crate::{Context as _, FAKES, Failure};

const PREFIX: &str = "domyjob-e2e-";
pub(crate) const FAILURE_LOG_BYTES: usize = 4096;
const FAILURE_LOG_FILES: usize = 8;
const FAILURE_SCAN_ENTRIES: usize = 128;
const FAILURE_SCAN_DEPTH: u8 = 4;

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
        for fake in FAKES.iter().chain(crate::os::extra_fakes()) {
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

    pub(crate) fn failure_diagnostics(&self) -> Result<String, Failure> {
        let mut remaining = FAILURE_SCAN_ENTRIES;
        let mut files = Vec::new();
        failure_logs(self.root(), 0, &mut remaining, &mut files)?;
        for name in &self.machines {
            failure_logs(
                &self.machine(name).state(),
                FAILURE_SCAN_DEPTH,
                &mut remaining,
                &mut files,
            )?;
        }
        let mut output = String::new();
        let read_limit = u64::try_from(FAILURE_LOG_BYTES.saturating_add(1))
            .context("representing the diagnostic byte limit")?;
        for path in &files {
            let mut bytes = Vec::new();
            fs::File::open(path)
                .context(&format!("opening diagnostic {}", path.display()))?
                .take(read_limit)
                .read_to_end(&mut bytes)
                .context(&format!("reading diagnostic {}", path.display()))?;
            let truncated = bytes.len() > FAILURE_LOG_BYTES;
            bytes.truncate(FAILURE_LOG_BYTES);
            let relative = path
                .strip_prefix(self.root())
                .context("diagnostic escaped its owned world")?;
            writeln!(
                output,
                "--- {} ---\n{}",
                relative.display(),
                String::from_utf8_lossy(&bytes)
            )
            .context("formatting owned failure diagnostics")?;
            if truncated {
                output.push_str("[log truncated]\n");
            }
        }
        if remaining == 0 || files.len() == FAILURE_LOG_FILES {
            output.push_str("[diagnostic scan limit reached]\n");
        }
        Ok(output)
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

fn failure_logs(
    directory: &Path,
    depth: u8,
    remaining: &mut usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), Failure> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Failure::new(format!(
                "listing diagnostics in {}: {error}",
                directory.display()
            )));
        }
    };
    for entry in entries {
        if *remaining == 0 || files.len() == FAILURE_LOG_FILES {
            break;
        }
        *remaining = remaining.saturating_sub(1);
        let entry = entry.context("listing an owned diagnostic entry")?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .context("inspecting an owned diagnostic")?;
        if kind.is_dir() && depth != 0 && entry.file_name() != "workspace" {
            failure_logs(&path, depth.saturating_sub(1), remaining, files)?;
        } else if kind.is_file()
            && path.file_name().is_some_and(|name| {
                name == "supervisor.log"
                    || name == "worker.log"
                    || name.to_string_lossy().ends_with("-serve.log")
            })
        {
            files.push(path);
        }
    }
    Ok(())
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
        let mut command = self.command(&full)?;
        command.stdin(Stdio::piped());
        let mut peer = Peer::spawn(command, label.clone()).context(&format!("starting {label}"))?;
        Ok(Mcp {
            input: peer.input(),
            peer,
            offset: 0,
            seen: Vec::new(),
            unclaimed: Vec::new(),
            requests: BTreeMap::new(),
            label,
        })
    }
}

fn execute(command: Command, label: String) -> Result<Run, Failure> {
    let mut peer = Peer::spawn(command, label.clone()).context(&format!("starting {label}"))?;
    let output = peer.wait(&label).context(&format!("waiting for {label}"))?;
    Ok(Run::new(label, &output))
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
    peer: Peer,
    input: Option<ChildStdin>,
    offset: usize,
    seen: Vec<String>,
    unclaimed: Vec<Value>,
    requests: BTreeMap<u64, Value>,
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
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&request)?;
        self.requests.insert(id, request);
        Ok(())
    }

    fn read_messages(&mut self) -> Result<(), Failure> {
        self.peer.read().context("reading MCP output")?;
        while let Some(line) = self
            .peer
            .read_line(&mut self.offset)
            .context("reading MCP output")?
        {
            let message = serde_json::from_str(&line).context(&format!(
                "{} printed a line that is not JSON: {line:?}",
                self.label
            ))?;
            self.seen.push(line);
            self.unclaimed.push(message);
        }
        Ok(())
    }

    pub(crate) fn take_response(&mut self, id: u64) -> Result<Option<Value>, Failure> {
        self.read_messages()?;
        let wanted = json!(id);
        if let Some(position) = self
            .unclaimed
            .iter()
            .position(|message| message.get("id") == Some(&wanted))
        {
            return Ok(Some(self.unclaimed.remove(position)));
        }
        Ok(None)
    }

    pub(crate) fn response(&mut self, id: u64) -> Result<Value, Failure> {
        let request = format!("{id}: {}", self.requests.get(&id).unwrap_or(&Value::Null));
        let mut pending = self.peer.waiting(&request);
        loop {
            let exited = self.peer.poll().context("observing the MCP process")?;
            let response = self
                .take_response(id)
                .map_err(|error| Failure::new(self.peer.failure(&request, error).to_string()))?;
            if let Some(message) = response {
                return Ok(message);
            }
            if let Some(status) = exited {
                return Err(Failure::new(
                    self.peer.failure(&request, status).to_string(),
                ));
            }
            pending.pause(self.peer.diagnostic(&request));
        }
    }

    pub(crate) fn request(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
    ) -> Result<Value, Failure> {
        self.start(id, method, params)?;
        self.response(id)
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
        let mut pending = self.peer.waiting("exit after MCP input EOF");
        let status = loop {
            let exited = self.peer.poll().context("observing MCP shutdown")?;
            self.read_messages()?;
            if let Some(status) = exited {
                break status;
            }
            pending.pause(self.peer.diagnostic("exit after MCP input EOF"));
        };
        ensure!(status.success(), "{} exited with {status}", self.label);
        ensure!(
            self.offset == self.peer.stdout().len(),
            "MCP exited with an incomplete frame: {}",
            self.peer.diagnostic("output EOF")
        );
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
    mut probe: impl FnMut() -> Result<Option<T>, Failure>,
) -> Result<T, Failure> {
    let mut pending = Wait::new(format!("e2e (pid {})", std::process::id()), what);
    loop {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        pending.pause("the requested state has not been published");
    }
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
