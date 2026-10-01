#![expect(
    unsafe_code,
    reason = "This leaf owns checked Windows kernel handles and SDK calls"
)]

use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsHandle as _, AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::os::windows::process::CommandExt as _;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
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
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, EVENT_MODIFY_STATE,
    GetExitCodeProcess, INFINITE, OpenEventW, OpenProcess, OpenThread,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread, SetEvent,
    THREAD_SUSPEND_RESUME, WaitForMultipleObjects, WaitForSingleObject,
};

mod sealed {
    pub(in super::super) trait Access {}
    pub(in super::super) trait JobState {}
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Signal(());
#[derive(Debug, Clone, Copy)]
pub(super) struct Wait(());
#[derive(Debug, Clone, Copy)]
pub(super) struct SignalWait(());
#[derive(Debug, Clone, Copy)]
pub(super) struct Watch(());
#[derive(Debug, Clone, Copy)]
pub(super) struct New(());
#[derive(Debug, Clone, Copy)]
pub(super) struct Configured(());

impl sealed::Access for Signal {}
impl sealed::Access for Wait {}
impl sealed::Access for SignalWait {}
impl sealed::Access for Watch {}
impl sealed::JobState for New {}
impl sealed::JobState for Configured {}

pub(super) trait CanSignal: sealed::Access {}
impl CanSignal for Signal {}
impl CanSignal for SignalWait {}

pub(super) trait CanWait: sealed::Access {}
impl CanWait for Wait {}
impl CanWait for SignalWait {}
impl CanWait for Watch {}

#[derive(Debug, Clone, Copy)]
pub(super) struct Success(());

fn checked_bool(status: i32) -> io::Result<Success> {
    if status == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Success(()))
    }
}

fn owned_handle(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

fn wide(text: &str) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = text.encode_utf16().collect();
    if value.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    value.push(0);
    Ok(value)
}

#[derive(Debug)]
pub(super) struct Event<A> {
    handle: OwnedHandle,
    access: PhantomData<A>,
}

impl Event<SignalWait> {
    pub(super) fn create(name: &str) -> io::Result<Self> {
        let name = wide(name)?;
        let handle = owned_handle(unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) })?;
        Ok(Self {
            handle,
            access: PhantomData,
        })
    }
}

impl Event<Signal> {
    pub(super) fn open(name: &str) -> io::Result<Self> {
        let name = wide(name)?;
        let handle = owned_handle(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) })?;
        Ok(Self {
            handle,
            access: PhantomData,
        })
    }
}

impl<A: CanSignal> Event<A> {
    pub(super) fn signal(&self) -> io::Result<Success> {
        checked_bool(unsafe { SetEvent(self.handle.as_raw_handle()) })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartWake {
    Ready,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitState {
    Signalled,
    Pending,
}

fn wait_one(handle: HANDLE, timeout: u32) -> io::Result<WaitState> {
    match unsafe { WaitForSingleObject(handle, timeout) } {
        WAIT_OBJECT_0 => Ok(WaitState::Signalled),
        WAIT_TIMEOUT => Ok(WaitState::Pending),
        _ => Err(io::Error::last_os_error()),
    }
}

#[derive(Debug)]
pub(super) struct EventRef<'event, A> {
    handle: &'event OwnedHandle,
    access: PhantomData<A>,
}

impl Event<SignalWait> {
    pub(super) const fn as_wait(&self) -> EventRef<'_, Wait> {
        EventRef {
            handle: &self.handle,
            access: PhantomData,
        }
    }
}

impl<A: CanWait> EventRef<'_, A> {
    #[cfg(test)]
    pub(super) fn is_signalled(&self) -> io::Result<bool> {
        wait_one(self.handle.as_raw_handle(), 0).map(|state| state == WaitState::Signalled)
    }

    pub(super) fn wait_for_start(&self, process: &Process<Watch>) -> io::Result<StartWake> {
        let handles = [self.handle.as_raw_handle(), process.handle.as_raw_handle()];
        let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if woke == WAIT_OBJECT_0 {
            Ok(StartWake::Ready)
        } else if woke == WAIT_OBJECT_0.saturating_add(1) {
            Ok(StartWake::Exited)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[derive(Debug)]
pub(super) struct Process<A> {
    handle: OwnedHandle,
    access: PhantomData<A>,
}

impl Process<Watch> {
    fn open(pid: u32) -> io::Result<Self> {
        let rights = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
        let handle = owned_handle(unsafe { OpenProcess(rights, 0, pid) })?;
        Ok(Self {
            handle,
            access: PhantomData,
        })
    }

    pub(super) fn duplicate_worker(child: &windows_spawn::Child) -> io::Result<Self> {
        child.as_handle().try_clone_to_owned().map(|handle| Self {
            handle,
            access: PhantomData,
        })
    }

    pub(super) fn exit_code(&self) -> io::Result<u32> {
        exit_code(&self.handle)
    }

    pub(super) const fn as_wait(&self) -> ProcessRef<'_, Wait> {
        ProcessRef {
            handle: &self.handle,
            access: PhantomData,
        }
    }
}

fn exit_code(process: &OwnedHandle) -> io::Result<u32> {
    let mut code = 0;
    checked_bool(unsafe { GetExitCodeProcess(process.as_raw_handle(), &raw mut code) })?;
    Ok(code)
}

#[derive(Debug)]
pub(super) struct ProcessRef<'process, A> {
    handle: &'process OwnedHandle,
    access: PhantomData<A>,
}

impl<A: CanWait> ProcessRef<'_, A> {
    pub(super) fn await_exit(&self) -> io::Result<Success> {
        match wait_one(self.handle.as_raw_handle(), INFINITE)? {
            WaitState::Signalled => Ok(Success(())),
            WaitState::Pending => Err(io::Error::other("the process wait did not complete")),
        }
    }
}

#[derive(Debug)]
pub(super) struct Job<S: sealed::JobState> {
    handle: OwnedHandle,
    state: PhantomData<S>,
}

impl Job<New> {
    pub(super) fn create() -> io::Result<Self> {
        let handle = owned_handle(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
        Ok(Self {
            handle,
            state: PhantomData,
        })
    }

    pub(super) fn configure(self) -> io::Result<Job<Configured>> {
        configure_job(&self.handle)?;
        Ok(Job {
            handle: self.handle,
            state: PhantomData,
        })
    }
}

fn configure_job(job: &OwnedHandle) -> io::Result<Success> {
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
        .map_err(io::Error::other)?;
    checked_bool(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const information).cast(),
            size,
        )
    })
}

#[derive(Debug)]
pub(super) struct AssignedJob<'child> {
    job: Job<Configured>,
    child: &'child Child,
}

impl Job<Configured> {
    pub(super) fn assign(self, child: &Child) -> io::Result<AssignedJob<'_>> {
        checked_bool(unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle(), child.as_raw_handle())
        })?;
        Ok(AssignedJob { job: self, child })
    }
}

#[derive(Debug)]
pub(super) struct WatchedJob<'child> {
    assigned: AssignedJob<'child>,
    process: Process<Watch>,
}

impl<'child> AssignedJob<'child> {
    pub(super) fn watch(self) -> io::Result<WatchedJob<'child>> {
        let process = Process::open(self.child.id())?;
        Ok(WatchedJob {
            assigned: self,
            process,
        })
    }
}

#[derive(Debug)]
pub(super) struct RunningJob {
    job: Job<Configured>,
    process: Process<Watch>,
}

impl WatchedJob<'_> {
    pub(super) fn resume(self) -> io::Result<RunningJob> {
        ThreadSnapshot::create()?.resume_process(self.assigned.child.id())?;
        Ok(RunningJob {
            job: self.assigned.job,
            process: self.process,
        })
    }
}

impl RunningJob {
    pub(super) fn await_leader(&self) -> io::Result<Success> {
        self.process.as_wait().await_exit()
    }

    pub(super) fn terminate(&self) -> io::Result<Success> {
        checked_bool(unsafe { TerminateJobObject(self.job.handle.as_raw_handle(), 1) })
    }
}

#[derive(Debug)]
struct Thread(OwnedHandle);

impl Thread {
    fn open(id: u32) -> io::Result<Self> {
        owned_handle(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, id) }).map(Self)
    }

    fn resume(&self) -> io::Result<Success> {
        checked_resume(unsafe { ResumeThread(self.0.as_raw_handle()) })
    }
}

fn checked_resume(count: u32) -> io::Result<Success> {
    if count == u32::MAX {
        Err(io::Error::last_os_error())
    } else {
        Ok(Success(()))
    }
}

fn more_threads(status: i32) -> io::Result<bool> {
    if status != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[derive(Debug)]
struct ThreadSnapshot(OwnedHandle);

impl ThreadSnapshot {
    fn create() -> io::Result<Self> {
        owned_handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }).map(Self)
    }

    fn resume_process(&self, pid: u32) -> io::Result<Success> {
        let size = u32::try_from(size_of::<THREADENTRY32>()).map_err(io::Error::other)?;
        let mut entry = THREADENTRY32 {
            dwSize: size,
            ..THREADENTRY32::default()
        };
        let mut resumed = 0_usize;
        let mut more =
            more_threads(unsafe { Thread32First(self.0.as_raw_handle(), &raw mut entry) })?;
        while more {
            if entry.th32OwnerProcessID == pid {
                Thread::open(entry.th32ThreadID)?.resume()?;
                resumed = resumed.saturating_add(1);
            }
            more = more_threads(unsafe { Thread32Next(self.0.as_raw_handle(), &raw mut entry) })?;
        }
        if resumed == 0 {
            Err(io::Error::other("the job process has no thread to start"))
        } else {
            Ok(Success(()))
        }
    }
}

pub(super) fn isolate(command: &mut Command) {
    command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(test)]
mod tests {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::{GENERIC_READ, STILL_ACTIVE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::JobObjects::OpenJobObjectW;
    use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, EVENT_MODIFY_STATE, GetCurrentThreadId, OpenEventW, OpenProcess, OpenThread,
        PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
        THREAD_QUERY_LIMITED_INFORMATION,
    };

    use super::{
        Event, Job, Process, WaitState, checked_bool, configure_job, exit_code, owned_handle,
        wait_one, wide,
    };

    fn named_resource() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().expect("owned resource name");
        let filename = root
            .path()
            .file_name()
            .expect("resource name")
            .to_string_lossy();
        let name = format!("Local\\domyjob-kernel-{}-{filename}", std::process::id());
        (root, name)
    }

    #[test]
    fn bool_and_thread_count_results_require_the_real_operation_to_succeed() {
        let (_root, name) = named_resource();
        let event = Event::create(&name).expect("owned event");
        let encoded = wide(&name).expect("event name");
        let read_only =
            owned_handle(unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, encoded.as_ptr()) })
                .expect("read-only event handle");
        let error = checked_bool(unsafe { SetEvent(read_only.as_raw_handle()) })
            .expect_err("modify-state access is required");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!event.as_wait().is_signalled().expect("unsignalled event"));
        Event::open(&name)
            .expect("signal capability")
            .signal()
            .expect("signal");
        assert!(event.as_wait().is_signalled().expect("signalled event"));
        let thread_id = unsafe { GetCurrentThreadId() };
        let query_only =
            owned_handle(unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, thread_id) })
                .expect("query-only thread handle");
        let resume_error =
            super::checked_resume(unsafe { super::ResumeThread(query_only.as_raw_handle()) })
                .expect_err("suspend-resume access is required");
        assert_eq!(resume_error.kind(), std::io::ErrorKind::PermissionDenied);
        super::Thread::open(thread_id)
            .expect("resume capability")
            .resume()
            .expect("zero suspend count is successful");
    }

    #[test]
    fn null_and_invalid_sentinels_retain_real_open_errors() {
        let (root, name) = named_resource();
        let encoded = wide(&name).expect("unused event name");
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, encoded.as_ptr()) };
        let event = owned_handle(event).map(std::mem::ManuallyDrop::new);
        assert_eq!(
            event.expect_err("missing event").kind(),
            std::io::ErrorKind::NotFound
        );
        let missing = root.path().join("missing-file");
        let filename: Vec<u16> = missing
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let file = unsafe {
            CreateFileW(
                filename.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        let file = owned_handle(file).map(std::mem::ManuallyDrop::new);
        assert_eq!(
            file.expect_err("missing file").kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn output_and_wait_contracts_preserve_access_denials() {
        let wait_only =
            owned_handle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, std::process::id()) })
                .expect("wait-only process handle");
        assert_eq!(
            exit_code(&wait_only)
                .expect_err("query access is required")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let process = Process::open(std::process::id()).expect("process watch capability");
        assert_eq!(
            process.exit_code().expect("current process exit code"),
            u32::try_from(STILL_ACTIVE).expect("active code")
        );
        let (_root, name) = named_resource();
        let event = Event::create(&name).expect("owned event");
        let encoded = wide(&name).expect("event name");
        let signal_only =
            owned_handle(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, encoded.as_ptr()) })
                .expect("signal-only event handle");
        assert_eq!(
            wait_one(signal_only.as_raw_handle(), 0)
                .expect_err("synchronize access is required")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            wait_one(event.handle.as_raw_handle(), 0).expect("pending event"),
            WaitState::Pending
        );
        event.signal().expect("signal event");
        assert_eq!(
            event
                .as_wait()
                .wait_for_start(&process)
                .expect("ready event"),
            super::StartWake::Ready
        );
    }

    #[test]
    fn job_configuration_checks_access_before_promoting_the_state() {
        let (_root, name) = named_resource();
        let encoded = wide(&name).expect("job name");
        let _job =
            owned_handle(unsafe { super::CreateJobObjectW(std::ptr::null(), encoded.as_ptr()) })
                .expect("owned named job");
        let read_only =
            owned_handle(unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, encoded.as_ptr()) })
                .expect("query-only job handle");
        assert_eq!(
            configure_job(&read_only)
                .expect_err("set-attributes access is required")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        Job::create()
            .expect("new anonymous job")
            .configure()
            .expect("configured job");
        let (_collision_root, collision) = named_resource();
        let collision = wide(&collision).expect("collision name");
        let _mutex = owned_handle(unsafe { CreateMutexW(std::ptr::null(), 0, collision.as_ptr()) })
            .expect("owned named mutex");
        let error =
            owned_handle(unsafe { super::CreateJobObjectW(std::ptr::null(), collision.as_ptr()) })
                .expect_err("job cannot reuse mutex name");
        assert_eq!(error.raw_os_error(), Some(6));
    }
}
