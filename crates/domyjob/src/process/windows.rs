#![expect(
    unsafe_code,
    reason = "Windows Job Objects, suspended starts, and readiness events require raw kernel handles"
)]

use std::io;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::process::CommandExt as _;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, EVENT_MODIFY_STATE,
    GetExitCodeProcess, INFINITE, OpenEventW, OpenProcess, OpenThread,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread, SetEvent,
    THREAD_SUSPEND_RESUME, WaitForMultipleObjects, WaitForSingleObject,
};

use super::{Guard, ProcessError, ReadyToken, Stop};

const WATCH: u32 = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;

#[derive(Debug)]
struct Owned(HANDLE);

impl Owned {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
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

fn event_name(token: &ReadyToken) -> String {
    format!("Local\\domyjob-ready-{}", token.as_str())
}

pub(super) fn announce_ready(token: Option<&ReadyToken>) -> Result<(), ProcessError> {
    let token = token.ok_or(ProcessError::InvalidReadyToken)?;
    let name = wide(&event_name(token));
    let event = Owned::new(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) })
        .map_err(ProcessError::Ready)?;
    if unsafe { SetEvent(event.0) } == 0 {
        return Err(ProcessError::Ready(io::Error::last_os_error()));
    }
    Ok(())
}

pub(super) fn launch_worker(
    arguments: &[&str],
    errors: Option<std::fs::File>,
) -> Result<(), ProcessError> {
    use std::os::windows::io::{AsHandle as _, IntoRawHandle as _};
    use windows_spawn::{CreationFlags, SpawnOptions, Stdio};

    let token = ReadyToken::fresh()?;
    let name = wide(&event_name(&token));
    let event = Owned::new(unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) })
        .map_err(ProcessError::Ready)?;
    let executable = std::env::current_exe().map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    let mut command = super::raw::detached(&executable);
    command
        .args(arguments)
        .arg("--ready-event")
        .arg(token.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errors.map_or_else(Stdio::null, Stdio::from));
    let detached = CreationFlags::NEW_PROCESS_GROUP | CreationFlags::BREAKAWAY_FROM_JOB;
    let child = match command.spawn_with(SpawnOptions::new().creation_flags(detached)) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // A job that forbids breakaway, such as a domyjob job or CI, keeps the worker inside it.
            command.spawn_with(SpawnOptions::new().creation_flags(CreationFlags::NEW_PROCESS_GROUP))
        }
        other => other,
    }
    .map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    let handle = child
        .as_handle()
        .try_clone_to_owned()
        .map_err(ProcessError::Wait)?;
    drop(child);
    let process = Owned::new(handle.into_raw_handle()).map_err(ProcessError::Wait)?;
    let handles = [event.0, process.0];
    let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
    if woke == WAIT_OBJECT_0 {
        return Ok(());
    }
    if woke != WAIT_OBJECT_0.saturating_add(1) {
        return Err(ProcessError::Wait(io::Error::last_os_error()));
    }
    let mut code = 0_u32;
    let known = unsafe { GetExitCodeProcess(process.0, &raw mut code) } != 0;
    Err(ProcessError::NotStarted(if known {
        format!("exit code {code:#x}")
    } else {
        "unknown exit code".to_owned()
    }))
}

pub(super) fn isolate(command: &mut Command) {
    command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

fn resume_threads(pid: u32) -> io::Result<()> {
    let snapshot = Owned::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let size = u32::try_from(size_of::<THREADENTRY32>()).map_err(io::Error::other)?;
    let mut entry = THREADENTRY32 {
        dwSize: size,
        ..THREADENTRY32::default()
    };
    let mut resumed = 0_usize;
    let mut more = unsafe { Thread32First(snapshot.0, &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            let thread =
                Owned::new(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })?;
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed = resumed.saturating_add(1);
        }
        more = unsafe { Thread32Next(snapshot.0, &raw mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other("the job process has no thread to start"));
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct Tree {
    job: Owned,
    process: Owned,
}

impl Tree {
    pub(super) fn adopt(child: &Child) -> io::Result<Self> {
        let job = Owned::new(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(io::Error::other)?;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const information).cast(),
                size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = Owned::new(unsafe { OpenProcess(WATCH, 0, child.id()) })?;
        resume_threads(child.id())?;
        Ok(Self { job, process })
    }

    pub(super) fn await_leader(&self) -> Result<(), ProcessError> {
        if unsafe { WaitForSingleObject(self.process.0, INFINITE) } == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(ProcessError::Wait(io::Error::last_os_error()))
        }
    }

    pub(super) fn kill_all(&self) -> Result<(), ProcessError> {
        if unsafe { TerminateJobObject(self.job.0, 1) } == 0 {
            Err(ProcessError::Signal(io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    /// Stop what remains of the job after its leader exited.
    pub(super) fn kill_remaining(&self) -> Result<(), ProcessError> {
        self.kill_all()
    }
}

#[derive(Debug)]
pub(super) struct Reaper;

/// A Job Object already ends the whole tree with its supervisor, so no guard process is needed.
impl Guard for Reaper {
    fn stand_guard(_tree: &Tree) -> Result<Self, ProcessError> {
        Ok(Self)
    }

    fn stand_down(self) {}

    fn reap(_group: i32) {}
}

/// The job output pipe; a Job Object ends every holder of its write end with the job.
pub(crate) type OutputReader = io::PipeReader;

#[derive(Debug)]
pub(crate) struct OutputStop;

pub(super) fn output_pipe() -> io::Result<(OutputReader, io::PipeWriter, OutputStop)> {
    let (reader, writer) = io::pipe()?;
    Ok((reader, writer, OutputStop))
}

impl Stop for OutputStop {
    fn stop(self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn terminate(pid: u32) -> Result<(), ProcessError> {
    let status = super::command("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(ProcessError::Signal)?;
    if status.success() || status.code() == Some(128) {
        Ok(())
    } else {
        Err(ProcessError::Signal(io::Error::other(format!(
            "taskkill failed: {status}"
        ))))
    }
}
