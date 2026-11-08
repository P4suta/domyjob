use std::path::Path;

use domyjob_core::domain::{Command, JobId, SubmissionId};
use domyjob_core::wire::{Input, Request};

pub(crate) fn home_request(submission: &SubmissionId, command: &str) -> Request {
    Request::Run {
        submission: submission.clone(),
        command: Command::try_from(vec![command.to_owned()]).unwrap(),
        input: Input::Home,
    }
}

pub(crate) fn home_job(store: &crate::store::Store, submission: &SubmissionId) -> (Request, JobId) {
    let request = home_request(submission, "true");
    let job = store.reserve(submission, &request, None).unwrap();
    (request, job)
}

pub(crate) fn completed_job(
    (store, job): (&crate::store::Store, &JobId),
    submission: &SubmissionId,
    request: &Request,
    state: &domyjob_core::state::JobState,
) {
    assert_eq!(&store.status(job).unwrap(), state);
    assert_eq!(&store.wait(job).unwrap(), state);
    assert_eq!(&store.reserve(submission, request, None).unwrap(), job);
}

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "test fixtures own their native file operations and readiness signal"
    )]

    use std::io;
    use std::path::Path;

    pub(super) fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    pub(super) fn is_dir(path: &Path) -> bool {
        path.is_dir()
    }

    pub(super) fn read_to_string(path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    pub(super) fn stdout() -> io::Stdout {
        io::stdout()
    }

    pub(super) fn set_permissions(
        path: &Path,
        permissions: std::fs::Permissions,
    ) -> io::Result<()> {
        std::fs::set_permissions(path, permissions)
    }
}

pub(crate) fn signal(bytes: &[u8]) {
    use std::io::Write as _;
    let mut output = raw::stdout();
    output.write_all(bytes).expect("signal the fixture event");
    output.flush().expect("flush the fixture event");
}

pub(crate) fn mkdir(path: &Path) {
    raw::create_dir_all(path).expect("create a fixture directory");
}

pub(crate) fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        mkdir(parent);
    }
    raw::write(path, bytes.as_ref()).expect("write a fixture file");
}

pub(crate) fn remove(path: &Path) {
    raw::remove_file(path).expect("remove a fixture file");
}

pub(crate) fn is_dir(path: &Path) -> bool {
    raw::is_dir(path)
}

pub(crate) fn read(path: &Path) -> String {
    raw::read_to_string(path).expect("read a fixture file")
}

pub(crate) fn outside_retirement(root: &Path) -> std::path::PathBuf {
    let outside = root.join("outside");
    crate::state_io::private_dir(&outside).unwrap();
    let child = outside.join("child");
    crate::state_io::private_dir(&child).unwrap();
    write(&child.join("canary"), b"outside-retirement");
    outside
}

pub(crate) fn protect(path: &Path) -> std::fs::Permissions {
    let original = std::fs::metadata(path)
        .expect("read a fixture's permissions")
        .permissions();
    let mut readonly = original.clone();
    readonly.set_readonly(true);
    raw::set_permissions(path, readonly).expect("protect a fixture");
    original
}

pub(crate) fn restore(path: &Path, permissions: std::fs::Permissions) {
    raw::set_permissions(path, permissions).expect("restore a fixture's permissions");
}
