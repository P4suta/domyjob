use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use tempfile::NamedTempFile;

use crate::peer::{Peer, Wait};
use crate::{Context as _, FAKES, Failure};

const PREFIX: &str = "domyjob-e2e-";
pub(crate) const FAILURE_LOG_BYTES: usize = 4096;
const FAILURE_LOG_FILES: usize = 8;
const FAILURE_SCAN_ENTRIES: usize = 128;

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

pub(crate) struct ServiceDiagnostics {
    pub(crate) text: String,
    observations: Vec<ServiceObservation>,
}

struct ServiceObservation {
    pid: u32,
    outcome: crate::diagnostics::Sampling,
    artifact: Option<PathBuf>,
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
        let mut logs = Vec::new();
        let mut budget = FAILURE_SCAN_ENTRIES;
        let mut limited = false;
        diagnostic_logs(self.root(), 0, &mut logs, (&mut budget, &mut limited))?;
        for name in &self.machines {
            diagnostic_logs(
                &self.machine(name).state(),
                1,
                &mut logs,
                (&mut budget, &mut limited),
            )?;
        }
        let mut notes = vec![format!("fixture artifacts: {}", self.root().display())];
        for path in logs {
            if let Some(tail) =
                crate::diagnostics::worker_log(&path).context("reading an owned failure log")?
            {
                notes.push(format!("--- {}\n{tail}", path.display()));
            }
        }
        if limited {
            notes.push("[diagnostic scan limit reached]".to_owned());
        }
        Ok(notes.join("\n"))
    }

    pub(crate) fn capture_service_diagnostics(&self) -> ServiceDiagnostics {
        let mut owned = self
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut notes = Vec::new();
        let mut observations = Vec::new();
        for (index, (label, child)) in owned.iter_mut().enumerate() {
            let pid = child.id();
            let observed = crate::diagnostics::sample_service(
                child,
                self.root(),
                crate::os::service_sampler,
                crate::peer::wait_sampler,
            );
            let evidence = match observed {
                Ok(evidence) => evidence,
                Err(error) => crate::diagnostics::SampleEvidence::collecting_failed(&error),
            };
            let saved = retain_sample(self.root(), index, pid, &evidence.text);
            notes.push(format!(
                "owned service {label} ({pid}): {:?}\n{}\nretaining sample: {saved:?}",
                evidence.outcome,
                evidence
                    .text
                    .chars()
                    .take(FAILURE_LOG_BYTES)
                    .collect::<String>(),
            ));
            observations.push(ServiceObservation {
                pid,
                outcome: evidence.outcome,
                artifact: saved.ok(),
            });
        }
        drop(owned);
        ServiceDiagnostics {
            text: notes.join("\n"),
            observations,
        }
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

fn diagnostic_logs(
    directory: &Path,
    depth: u8,
    logs: &mut Vec<PathBuf>,
    limits: (&mut usize, &mut bool),
) -> Result<(), Failure> {
    let (budget, limited) = limits;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Failure::new(format!("listing diagnostics: {error}"))),
    };
    for entry in entries {
        if *budget == 0 {
            *limited = true;
            break;
        }
        *budget = budget.saturating_sub(1);
        let entry = entry.context("reading diagnostic entries")?;
        let kind = entry.file_type().context("inspecting diagnostic entries")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let wanted = kind.is_file()
            && ((depth == 0 && name.ends_with("-service-serve.log"))
                || (depth == 4 && name == "supervisor.log"));
        if wanted {
            if logs.len() < FAILURE_LOG_FILES {
                logs.push(entry.path());
            } else {
                *limited = true;
            }
        } else if kind.is_dir()
            && ((depth == 1 && name.starts_with("runner-"))
                || (depth == 2 && name == "jobs")
                || depth == 3)
        {
            diagnostic_logs(
                &entry.path(),
                depth.saturating_add(1),
                logs,
                (budget, limited),
            )?;
        }
    }
    Ok(())
}

fn retain_sample(directory: &Path, index: usize, pid: u32, text: &str) -> io::Result<PathBuf> {
    let mut artifact = tempfile::Builder::new()
        .prefix(&format!("service-{index}-{pid}-"))
        .suffix(".sample.txt")
        .tempfile_in(directory)?;
    artifact.as_file_mut().write_all(text.as_bytes())?;
    artifact.as_file().sync_all()?;
    let (closed, path) = artifact.keep().map_err(|error| error.error)?;
    drop(closed);
    Ok(path)
}

fn start_diagnostic_fixture(world: &World, machine: &Machine<'_>) -> Result<u32, Failure> {
    let child = Command::new(world.program("sleeper"))
        .arg("120000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the owned diagnostic fixture")?;
    let pid = child.id();
    world
        .background
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((format!("diagnostic sleeper on {}", machine.name), child));
    Ok(pid)
}

pub(crate) fn service_diagnostic_fixture(world: &World) -> Result<(), Failure> {
    let machine = world.machine("alpha");
    let pid = start_diagnostic_fixture(world, &machine)?;
    let diagnostics = world.capture_service_diagnostics();
    let evidence = &diagnostics.text;
    let mut owned = world
        .background
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let service = owned
        .iter_mut()
        .find(|(_, child)| child.id() == pid)
        .ok_or_else(|| Failure::new("the owned service child was removed before sampling"))?;
    let ended = service
        .1
        .try_wait()
        .context("checking the owned sampled service")?;
    drop(owned);
    ensure!(
        ended.is_none(),
        "sampling terminated the owned service: {evidence}"
    );
    let [observation] = diagnostics.observations.as_slice() else {
        return Err(Failure::new(format!(
            "expected one owned service sample: {evidence}"
        )));
    };
    ensure!(
        observation.pid == pid,
        "the sample belongs to another process: {evidence}"
    );
    let artifact = observation
        .artifact
        .as_ref()
        .ok_or_else(|| Failure::new(format!("the sample was not retained: {evidence}")))?;
    let retained = crate::diagnostics::worker_log(artifact)
        .context("reading the retained service sample")?
        .ok_or_else(|| Failure::new("the service sample artifact was not retained"))?;
    ensure!(!retained.is_empty(), "the retained sample is empty");
    let another = retain_sample(world.root(), 0, pid, "second capture control")
        .context("retaining another diagnostic generation")?;
    ensure!(
        another != *artifact,
        "a later capture replaced the first artifact"
    );
    ensure!(
        crate::diagnostics::worker_log(artifact).context("rechecking the first sample")?
            == Some(retained),
        "a later capture changed the first diagnostic artifact"
    );
    match observation.outcome {
        crate::diagnostics::Sampling::Unsupported => {
            eprintln!(
                "native stack sampling is unsupported on this platform; owned-service diagnostic refusal and retention verified"
            );
            return Ok(());
        }
        crate::diagnostics::Sampling::Refused(cause) => {
            eprintln!(
                "native stack sampling was refused ({cause:?}); bounded failure evidence retained: {}",
                artifact.display()
            );
            return Ok(());
        }
        crate::diagnostics::Sampling::ServiceExited => {
            return Err(Failure::new(format!(
                "the service exited before sampling: {evidence}"
            )));
        }
        crate::diagnostics::Sampling::Captured => {}
    }
    ensure!(
        evidence.contains("Call graph:")
            && evidence.contains(&format!("Process:         sleeper [{pid}]")),
        "the native sampler did not capture the exact owned service: {evidence}"
    );
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
            .env("DOMYJOB_REFRESH", "never");
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
        execute(command, self.label(arguments), None)
    }

    pub(crate) fn run_without_output_reader(&self, arguments: &[&str]) -> Result<Run, Failure> {
        let (reader, writer) = io::pipe().context("creating the closed output fixture")?;
        drop(reader);
        let mut command = self.command(arguments)?;
        command.stdin(Stdio::null()).stderr(Stdio::piped());
        execute(command, self.label(arguments), Some(Stdio::from(writer)))
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
}

fn execute(command: Command, label: String, destination: Option<Stdio>) -> Result<Run, Failure> {
    let mut peer =
        Peer::spawn(command, label.clone(), destination).context(&format!("starting {label}"))?;
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
