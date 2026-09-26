use std::io::{PipeReader, PipeWriter, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("starting {what}: {source}")]
    Spawn {
        what: &'static str,
        source: std::io::Error,
    },
    #[error("the supervisor exited before it took charge of the job ({0})")]
    NotStarted(String),
    #[error("waiting for the command: {0}")]
    Wait(std::io::Error),
    #[error("signalling the command: {0}")]
    Signal(std::io::Error),
    #[error("collecting the command's output: {0}")]
    Output(std::io::Error),
    #[error("announcing that the supervisor is ready: {0}")]
    Ready(std::io::Error),
    #[error("the process state lock was poisoned")]
    Poisoned,
}

pub use platform::Readiness;

pub fn own_executable() -> std::io::Result<std::path::PathBuf> {
    match crate::platform::RUNNING_EXECUTABLE {
        Some(running) => Ok(std::path::PathBuf::from(running)),
        None => std::env::current_exe(),
    }
}

pub fn launch(invocation: &crate::spawn::Invocation) -> Result<(), ProcError> {
    platform::launch(invocation)
}

pub fn spawn_detached(invocation: &crate::spawn::Invocation) -> Result<(), ProcError> {
    platform::spawn_detached(invocation)
}

#[derive(Debug)]
enum Child {
    Running(std::process::Child),
    Reaped,
}

#[derive(Debug)]
pub struct Group {
    child: Mutex<Child>,
    tree: platform::Tree,
    id: u32,
    reaper: Mutex<Option<platform::Reaper>>,
}

#[cfg_attr(
    windows,
    expect(
        clippy::missing_const_for_fn,
        reason = "on Windows the job object reaps the tree, so there is nothing to do here"
    )
)]
pub fn reap(group: i32) {
    platform::reap(group);
}

pub fn terminate(pid: u32) {
    platform::terminate(pid);
}

impl Group {
    pub fn spawn(mut command: Command, output: PipeWriter) -> Result<Self, ProcError> {
        let spawn_error = |source| ProcError::Spawn {
            what: "the command",
            source,
        };
        let errors = output.try_clone().map_err(spawn_error)?;
        command.stdin(Stdio::null()).stdout(output).stderr(errors);
        platform::isolate(&mut command);
        let mut child = command.spawn().map_err(spawn_error)?;
        drop(command);
        let id = child.id();
        let tree = match platform::Tree::adopt(&child) {
            Ok(tree) => tree,
            Err(adopting) => {
                return Err(match child.kill().and_then(|()| child.wait()) {
                    Ok(_) => spawn_error(adopting),
                    Err(killing) => spawn_error(std::io::Error::other(format!(
                        "{adopting}, and the half-started command could not be stopped: {killing}"
                    ))),
                });
            }
        };
        let reaper = match platform::Reaper::stand_guard(&tree) {
            Ok(reaper) => Some(reaper),
            Err(_unguarded_but_still_waited_on) => None,
        };
        Ok(Self {
            child: Mutex::new(Child::Running(child)),
            tree,
            id,
            reaper: Mutex::new(reaper),
        })
    }

    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    pub fn wait(&self) -> Result<ExitStatus, ProcError> {
        self.tree.await_leader(self.id)?;
        let mut guard = self.child.lock().map_err(|_poisoned| ProcError::Poisoned)?;
        let state = std::mem::replace(&mut *guard, Child::Reaped);
        let Child::Running(mut child) = state else {
            return Err(ProcError::Wait(std::io::Error::other(
                "the command was already reaped",
            )));
        };
        self.tree.kill_all()?;
        let status = child.wait().map_err(ProcError::Wait);
        drop(guard);
        let reaper = match self.reaper.lock() {
            Ok(mut held) => held.take(),
            Err(_poisoned) => None,
        };
        if let Some(reaper) = reaper {
            reaper.stand_down();
        }
        status
    }

    pub fn kill(&self) -> Result<(), ProcError> {
        let guard = self.child.lock().map_err(|_poisoned| ProcError::Poisoned)?;
        match &*guard {
            Child::Running(_) => self.tree.kill_all(),
            Child::Reaped => Ok(()),
        }
    }
}

enum Next {
    Readable,
    #[cfg_attr(
        windows,
        expect(
            dead_code,
            reason = "on Windows the Job Object closes every writer, so a read always ends by itself"
        )
    )]
    Stopped,
}

#[derive(Debug)]
pub struct Collector {
    reader: PipeReader,
    stop: platform::StopReceiver,
}

#[derive(Debug)]
pub struct Stopper(platform::StopSender);

pub fn output_pipe() -> Result<(Collector, Stopper, PipeWriter), ProcError> {
    let (reader, writer) = std::io::pipe().map_err(ProcError::Output)?;
    let (stop, stopper) = platform::stop_pair()?;
    Ok((Collector { reader, stop }, Stopper(stopper), writer))
}

impl Collector {
    pub fn run(mut self, sink: &mut dyn FnMut(&[u8])) -> Result<(), ProcError> {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match platform::next(&self.reader, &self.stop)? {
                Next::Readable => {}
                Next::Stopped => {
                    return platform::drain(&mut self.reader, &mut buffer, sink);
                }
            }
            let read = match self.reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(ProcError::Output(error)),
            };
            match buffer.get(..read) {
                Some([]) | None => return Ok(()),
                Some(chunk) => sink(chunk),
            }
        }
    }
}

impl Stopper {
    #[cfg_attr(
        windows,
        expect(
            clippy::missing_const_for_fn,
            reason = "on Unix stopping writes to a pipe; only the Windows body happens to be empty"
        )
    )]
    pub fn stop(self) -> Result<(), ProcError> {
        platform::stop(self.0)
    }
}

#[must_use]
pub fn exit_code(status: ExitStatus) -> i32 {
    match status.code() {
        Some(code) => code,
        None => platform::signal_code(status),
    }
}

#[cfg(unix)]
mod platform {
    use std::io::{PipeReader, PipeWriter, Read, Write};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, ExitStatus, Stdio};

    use rustix::event::{PollFd, PollFlags};
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group};

    use super::{Next, ProcError};
    use crate::domain::BlobId;

    #[derive(Debug)]
    pub struct Readiness(std::io::Stdout);

    impl Readiness {
        #[must_use]
        pub fn from_parent(event: Option<&BlobId>) -> Option<Self> {
            match event {
                None => Some(Self(std::io::stdout())),
                Some(_) => None,
            }
        }

        #[cfg(test)]
        #[must_use]
        pub fn unwatched() -> Self {
            Self(std::io::stdout())
        }

        pub fn announce(self) -> Result<(), ProcError> {
            let mut out = self.0.lock();
            out.write_all(b"R")
                .and_then(|()| out.flush())
                .map_err(ProcError::Ready)
        }
    }

    pub(super) fn launch(invocation: &crate::spawn::Invocation) -> Result<(), ProcError> {
        let mut command = invocation.command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().map_err(|source| ProcError::Spawn {
            what: "the supervisor",
            source,
        })?;
        drop(command);
        let mut signal = [0u8; 1];
        let read = match child.stdout.take() {
            Some(mut out) => out.read(&mut signal).map_err(ProcError::Ready)?,
            None => 0,
        };
        std::thread::spawn(move || match child.wait() {
            Ok(_) | Err(_) => {}
        });
        match read {
            1 => Ok(()),
            _ => Err(ProcError::NotStarted("it closed its output".to_owned())),
        }
    }

    pub(super) fn spawn_detached(invocation: &crate::spawn::Invocation) -> Result<(), ProcError> {
        let mut command = invocation.command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().map_err(|source| ProcError::Spawn {
            what: "a detached process",
            source,
        })?;
        std::thread::spawn(move || match child.wait() {
            Ok(_) | Err(_) => {}
        });
        Ok(())
    }

    pub(super) fn isolate(command: &mut Command) {
        command.process_group(0);
    }

    #[derive(Debug)]
    pub(super) struct Tree {
        group: Pid,
    }

    impl Tree {
        pub(super) fn adopt(child: &Child) -> std::io::Result<Self> {
            let raw = i32::try_from(child.id()).map_err(std::io::Error::other)?;
            let group = Pid::from_raw(raw)
                .ok_or_else(|| std::io::Error::other("the command has no process id"))?;
            Ok(Self { group })
        }

        pub(super) fn await_leader(&self, _id: u32) -> Result<(), ProcError> {
            loop {
                match rustix::process::waitid(
                    WaitId::Pid(self.group),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
                ) {
                    Ok(_) => return Ok(()),
                    Err(Errno::INTR) => {}
                    Err(errno) => return Err(ProcError::Wait(errno.into())),
                }
            }
        }

        pub(super) fn kill_all(&self) -> Result<(), ProcError> {
            match kill_process_group(self.group, Signal::KILL) {
                Ok(()) | Err(Errno::SRCH | Errno::PERM) => Ok(()),
                Err(errno) => Err(ProcError::Signal(errno.into())),
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct Reaper(std::process::ChildStdin);

    impl Reaper {
        pub(super) fn stand_guard(tree: &Tree) -> Result<Self, ProcError> {
            let spawning = |source| ProcError::Spawn {
                what: "the reaper",
                source,
            };
            let exe = super::own_executable().map_err(spawning)?;
            let group = u64::try_from(tree.group.as_raw_nonzero().get()).map_err(|_negative| {
                spawning(std::io::Error::other("the process group id is negative"))
            })?;
            let mut command = crate::spawn::Invocation::new(
                crate::template::Arg::path(&exe),
                vec![
                    crate::template::Arg::literal("node"),
                    crate::template::Arg::literal("--reap"),
                    crate::template::Arg::number(group),
                ],
            )
            .command();
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            let mut child = command.spawn().map_err(spawning)?;
            let Some(stdin) = child.stdin.take() else {
                return Err(spawning(std::io::Error::other("the reaper has no input")));
            };
            std::thread::spawn(move || match child.wait() {
                Ok(_) | Err(_) => {}
            });
            Ok(Self(stdin))
        }

        pub(super) fn stand_down(mut self) {
            match self.0.write_all(b"d") {
                Ok(()) | Err(_) => {}
            }
        }
    }

    pub(super) fn terminate(pid: u32) {
        let raw = match i32::try_from(pid) {
            Ok(raw) => raw,
            Err(_beyond_pid_range) => return,
        };
        let Some(process) = Pid::from_raw(raw) else {
            return;
        };
        match rustix::process::kill_process(process, Signal::KILL) {
            Ok(()) | Err(_) => {}
        }
    }

    pub(super) fn reap(group: i32) {
        let mut heard = Vec::new();
        let read = std::io::stdin().lock().read_to_end(&mut heard);
        match read {
            Ok(_) | Err(_) => {}
        }
        if heard.contains(&b'd') {
            return;
        }
        if let Some(group) = Pid::from_raw(group) {
            match kill_process_group(group, Signal::KILL) {
                Ok(()) | Err(_) => {}
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct StopReceiver(PipeReader);

    #[derive(Debug)]
    pub(super) struct StopSender(PipeWriter);

    pub(super) fn stop_pair() -> Result<(StopReceiver, StopSender), ProcError> {
        let (reader, writer) = std::io::pipe().map_err(ProcError::Output)?;
        Ok((StopReceiver(reader), StopSender(writer)))
    }

    pub(super) fn stop(mut sender: StopSender) -> Result<(), ProcError> {
        match sender.0.write_all(b"S") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(ProcError::Output(error)),
        }
    }

    pub(super) fn next(data: &PipeReader, stop: &StopReceiver) -> Result<Next, ProcError> {
        loop {
            let mut fds = [
                PollFd::new(data, PollFlags::IN),
                PollFd::new(&stop.0, PollFlags::IN),
            ];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(errno) => return Err(ProcError::Output(errno.into())),
            }
            match fds.map(|fd| !fd.revents().is_empty()) {
                [true, _] => return Ok(Next::Readable),
                [false, true] => return Ok(Next::Stopped),
                [false, false] => {}
            }
        }
    }

    pub(super) fn drain(
        data: &mut PipeReader,
        buffer: &mut [u8],
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<(), ProcError> {
        rustix::io::ioctl_fionbio(&*data, true).map_err(|e| ProcError::Output(e.into()))?;
        loop {
            match data.read(buffer) {
                Ok(read) => match buffer.get(..read) {
                    Some([]) | None => return Ok(()),
                    Some(chunk) => sink(chunk),
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(ProcError::Output(error)),
            }
        }
    }

    pub(super) fn signal_code(status: ExitStatus) -> i32 {
        match status.signal() {
            Some(signal) => signal.saturating_add(128),
            None => -1,
        }
    }
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Job Objects, suspended starts, and kernel events have no safe Rust interface"
)]
mod platform {
    use std::io::{PipeReader, Read};
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, ExitStatus, Stdio};

    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW,
        EVENT_MODIFY_STATE, GetExitCodeProcess, INFINITE, OpenEventW, OpenProcess, OpenThread,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, ResumeThread,
        SetEvent, THREAD_SUSPEND_RESUME, TerminateProcess, WaitForMultipleObjects,
        WaitForSingleObject,
    };

    use super::{Next, ProcError};
    use crate::domain::BlobId;

    const WATCH: u32 = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;

    #[derive(Debug)]
    struct Owned(HANDLE);

    impl Owned {
        fn new(handle: HANDLE) -> std::io::Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn event_name(token: &BlobId) -> String {
        format!("Local\\domyjob-ready-{token}")
    }

    #[derive(Debug)]
    pub struct Readiness(BlobId);

    impl Readiness {
        #[must_use]
        pub fn from_parent(event: Option<&BlobId>) -> Option<Self> {
            let token = event?;
            Some(Self(token.clone()))
        }

        #[cfg(test)]
        #[must_use]
        pub fn unwatched() -> Self {
            Self(BlobId::of(b"nobody waits for this supervisor"))
        }

        pub fn announce(self) -> Result<(), ProcError> {
            let name = wide(&event_name(&self.0));
            let event = Owned::new(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) })
                .map_err(ProcError::Ready)?;
            if unsafe { SetEvent(event.0) } == 0 {
                return Err(ProcError::Ready(std::io::Error::last_os_error()));
            }
            Ok(())
        }
    }

    fn fresh_event() -> Result<(BlobId, Owned), ProcError> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random)
            .map_err(|e| ProcError::Ready(std::io::Error::other(e.to_string())))?;
        let token = BlobId::of(&random);
        let wide_name = wide(&event_name(&token));
        let event = Owned::new(unsafe { CreateEventW(std::ptr::null(), 1, 0, wide_name.as_ptr()) })
            .map_err(ProcError::Ready)?;
        Ok((token, event))
    }

    pub(super) fn launch(invocation: &crate::spawn::Invocation) -> Result<(), ProcError> {
        use crate::template::Arg;
        let (token, event) = fresh_event()?;
        let announced = invocation
            .clone()
            .arg(Arg::literal("--ready-event"))
            .arg(Arg::word(&token));
        let process = start_outside_the_session(&announced)?;
        let handles = [event.0, process.0];
        let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if woke == WAIT_OBJECT_0 {
            return Ok(());
        }
        if woke != WAIT_OBJECT_0.saturating_add(1) {
            return Err(ProcError::Wait(std::io::Error::last_os_error()));
        }
        let mut code = 0u32;
        let known = unsafe { GetExitCodeProcess(process.0, &raw mut code) } != 0;
        Err(ProcError::NotStarted(if known {
            format!("exit code {code:#x}")
        } else {
            "exit code unknown".to_owned()
        }))
    }

    pub(super) fn spawn_detached(invocation: &crate::spawn::Invocation) -> Result<(), ProcError> {
        start_outside_the_session(invocation).map(drop)
    }

    fn start_outside_the_session(
        invocation: &crate::spawn::Invocation,
    ) -> Result<Owned, ProcError> {
        match direct(invocation) {
            Ok(process) => Ok(process),
            Err(first) if inside_remote_session() => escape_job(invocation).map_err(|second| {
                ProcError::Spawn {
                    what: "the supervisor",
                    source: std::io::Error::other(format!(
                        "starting it directly failed ({first}), and so did starting it through WMI ({second})"
                    )),
                }
            }),
            Err(first) => Err(first),
        }
    }

    fn direct(invocation: &crate::spawn::Invocation) -> Result<Owned, ProcError> {
        use std::os::windows::io::{AsHandle, IntoRawHandle};
        use windows_spawn::{CreationFlags, SpawnOptions, Stdio as Nothing};
        let spawn_error = |source| ProcError::Spawn {
            what: "the supervisor",
            source,
        };
        let mut command = invocation.windows_command();
        command
            .stdin(Nothing::null())
            .stdout(Nothing::null())
            .stderr(Nothing::null());
        let flags = CreationFlags::DETACHED_PROCESS
            | CreationFlags::NEW_PROCESS_GROUP
            | CreationFlags::BREAKAWAY_FROM_JOB;
        let child = command
            .spawn_with(SpawnOptions::new().creation_flags(flags))
            .map_err(spawn_error)?;
        let process = child
            .as_handle()
            .try_clone_to_owned()
            .map_err(ProcError::Wait)?;
        drop(child);
        Owned::new(process.into_raw_handle()).map_err(ProcError::Wait)
    }

    fn inside_remote_session() -> bool {
        std::env::var_os("SSH_CONNECTION").is_some()
    }

    fn escape_job(invocation: &crate::spawn::Invocation) -> Result<Owned, ProcError> {
        use crate::template::Arg;
        let failed = |detail: &str| ProcError::Spawn {
            what: "the supervisor",
            source: std::io::Error::other(detail.to_owned()),
        };
        let mut line = crate::shell::msvc_program(invocation.program().as_arg_str())
            .ok_or_else(|| failed("the program path contains a quote"))?;
        for arg in invocation.args() {
            line.push(' ');
            line.push_str(&crate::shell::msvc_quote(arg.as_arg_str()));
        }
        let dir = match invocation.dir() {
            Some(dir) => {
                Arg::authorized_job_text(crate::shell::powershell_quote(&dir.display().to_string()))
            }
            None => Arg::literal("$null"),
        };
        let script = Arg::concat(&[
            Arg::literal(
                "$s = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{CreateFlags=[uint32]4}; $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{CommandLine=",
            ),
            Arg::authorized_job_text(crate::shell::powershell_quote(&line)),
            Arg::literal("; CurrentDirectory="),
            dir,
            Arg::literal(
                "; ProcessStartupInformation=$s}; if ($r.ReturnValue -ne 0) { exit 1 }; [Console]::Out.Write($r.ProcessId)",
            ),
        ]);
        let wmi = crate::spawn::Invocation::new(
            Arg::literal("powershell"),
            vec![
                Arg::literal("-NoProfile"),
                Arg::literal("-NonInteractive"),
                Arg::literal("-InputFormat"),
                Arg::literal("None"),
                Arg::literal("-EncodedCommand"),
                Arg::powershell_encoded(&script),
            ],
        );
        let asking = |source| ProcError::Spawn {
            what: "the supervisor through WMI",
            source,
        };
        let helper = wmi
            .command()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(asking)?;
        let helper_id = helper.id();
        let output =
            crate::liveness::unless_abandoned(crate::liveness::Helper::Process(helper_id), || {
                helper.wait_with_output()
            })
            .map_err(asking)?;
        if !output.status.success() {
            return Err(failed("WMI refused to create the supervisor"));
        }
        let pid: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_garbled| failed("WMI did not report a process id"))?;
        let process = Owned::new(unsafe { OpenProcess(WATCH | PROCESS_TERMINATE, 0, pid) })
            .map_err(ProcError::Wait)?;
        if let Err(resuming) = resume_threads(pid) {
            if unsafe { TerminateProcess(process.0, 1) } == 0 {
                return Err(ProcError::Wait(std::io::Error::other(format!(
                    "{resuming}, and the suspended supervisor could not be stopped: {}",
                    std::io::Error::last_os_error()
                ))));
            }
            return Err(ProcError::Wait(resuming));
        }
        Ok(process)
    }

    fn resume_threads(pid: u32) -> std::io::Result<()> {
        let snapshot = Owned::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
        let size = u32::try_from(size_of::<THREADENTRY32>()).map_err(std::io::Error::other)?;
        let mut entry = THREADENTRY32 {
            dwSize: size,
            ..THREADENTRY32::default()
        };
        let mut resumed = 0usize;
        let mut more = unsafe { Thread32First(snapshot.0, &raw mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = Owned::new(unsafe {
                    OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID)
                })?;
                if unsafe { ResumeThread(thread.0) } == u32::MAX {
                    return Err(std::io::Error::last_os_error());
                }
                resumed = resumed.saturating_add(1);
            }
            more = unsafe { Thread32Next(snapshot.0, &raw mut entry) } != 0;
        }
        if resumed == 0 {
            return Err(std::io::Error::other(
                "the new process has no thread to start",
            ));
        }
        Ok(())
    }

    pub(super) fn isolate(command: &mut Command) {
        command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    #[derive(Debug)]
    pub(super) struct Reaper;

    impl Reaper {
        #[expect(
            clippy::unnecessary_wraps,
            reason = "the same signature as the Unix reaper, which can fail to start"
        )]
        pub(super) const fn stand_guard(_tree: &Tree) -> Result<Self, ProcError> {
            Ok(Self)
        }

        #[expect(
            clippy::unused_self,
            reason = "the job object already ends the tree with the supervisor, so standing down is a no-op"
        )]
        pub(super) const fn stand_down(self) {}
    }

    pub(super) const fn reap(_group: i32) {}

    pub(super) fn terminate(pid: u32) {
        if let Ok(process) = Owned::new(unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) }) {
            unsafe { TerminateProcess(process.0, 1) };
        }
    }

    #[derive(Debug)]
    pub(super) struct Tree {
        job: Owned,
        process: Owned,
    }

    impl Tree {
        pub(super) fn adopt(child: &Child) -> std::io::Result<Self> {
            let job = Owned::new(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(std::io::Error::other)?;
            let limited = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size,
                )
            };
            if limited == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let process = Owned::new(unsafe { OpenProcess(WATCH, 0, child.id()) })?;
            resume_threads(child.id())?;
            Ok(Self { job, process })
        }

        pub(super) fn await_leader(&self, _id: u32) -> Result<(), ProcError> {
            if unsafe { WaitForSingleObject(self.process.0, INFINITE) } == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err(ProcError::Wait(std::io::Error::last_os_error()))
            }
        }

        pub(super) fn kill_all(&self) -> Result<(), ProcError> {
            if unsafe { TerminateJobObject(self.job.0, 1) } == 0 {
                return Err(ProcError::Signal(std::io::Error::last_os_error()));
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    pub(super) struct StopReceiver;

    #[derive(Debug)]
    pub(super) struct StopSender;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "shares the Unix signature; on Windows the Job Object closes every writer"
    )]
    pub(super) const fn stop_pair() -> Result<(StopReceiver, StopSender), ProcError> {
        Ok((StopReceiver, StopSender))
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "shares the Unix signature; on Windows the Job Object closes every writer"
    )]
    pub(super) const fn stop(_sender: StopSender) -> Result<(), ProcError> {
        Ok(())
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "shares the Unix signature; on Windows a blocking read ends when the Job Object closes every writer"
    )]
    pub(super) const fn next(_data: &PipeReader, _stop: &StopReceiver) -> Result<Next, ProcError> {
        Ok(Next::Readable)
    }

    pub(super) fn drain(
        data: &mut PipeReader,
        buffer: &mut [u8],
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<(), ProcError> {
        loop {
            let read = data.read(buffer).map_err(ProcError::Output)?;
            match buffer.get(..read) {
                Some([]) | None => return Ok(()),
                Some(chunk) => sink(chunk),
            }
        }
    }

    pub(super) const fn signal_code(_status: ExitStatus) -> i32 {
        -1
    }
}
