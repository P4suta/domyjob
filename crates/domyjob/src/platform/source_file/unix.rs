use std::ffi::OsStr;
use std::fs::File;
use std::io;

use rustix::fs::{Mode, OFlags};

pub(super) fn directory(parent: &File, name: &OsStr) -> io::Result<File> {
    cap_primitives::fs::open_dir_nofollow(parent, std::path::Path::new(name))
}

#[cfg(test)]
pub(super) const fn rename_blocked(_error: &io::Error) -> bool {
    false
}

pub(super) fn open(parent: &File, name: &OsStr) -> io::Result<File> {
    let file = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(file.into())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::source_archive::ArchiveError;
    use crate::source_archive::tests::file_fixture;

    #[test]
    fn final_symlink_replacement_never_reads_repository_metadata() {
        let fixture = file_fixture("file");
        crate::testing::write(&fixture.root.path().join(".git/config"), b"secret");
        let error = fixture.replaced(|path| {
            std::os::unix::fs::symlink(Path::new(".git/config"), path).unwrap();
        });
        assert!(matches!(error, ArchiveError::Io { .. }));
    }

    #[test]
    fn a_fifo_replacing_a_regular_file_is_refused_without_waiting_for_a_writer() {
        const CHILD: &str = "DOMYJOB_SOURCE_FIFO_TEST";
        const RESULT: &str = "DOMYJOB_SOURCE_FIFO_RESULT";
        const COMPLETED: &[u8] = b"refused";
        const WAIT_SECONDS: u64 = 3;
        const POLL_MILLIS: u64 = 5;
        if std::env::var_os(CHILD).is_none() {
            let completion = tempfile::NamedTempFile::new().unwrap();
            let mut child = crate::process::command(std::env::current_exe().unwrap())
                .args(["--exact", "source_archive::source_file::unix::tests::a_fifo_replacing_a_regular_file_is_refused_without_waiting_for_a_writer", "--nocapture"])
                .env(CHILD, "1").env(RESULT, completion.path()).spawn().unwrap();
            let deadline = crate::platform::clock::Deadline::after_seconds(WAIT_SECONDS);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "FIFO refusal failed: {status}");
                    assert_eq!(
                        crate::bounded::read(
                            completion.as_file().try_clone().unwrap(),
                            COMPLETED.len()
                        )
                        .unwrap()
                        .unwrap(),
                        COMPLETED,
                        "the child must execute the actual FIFO refusal"
                    );
                    return;
                }
                if deadline.expired() {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("the source read blocked on a FIFO without a writer");
                }
                crate::platform::clock::pause_millis(POLL_MILLIS);
            }
        }
        let fixture = file_fixture("file");
        let error = fixture.replaced(|path| {
            let status = crate::process::command("mkfifo")
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success(), "FIFO creation failed: {status}");
        });
        assert!(matches!(error, ArchiveError::Io { .. }));
        let completion = std::env::var_os(RESULT).expect("owned completion marker");
        crate::testing::write(Path::new(&completion), COMPLETED);
    }
}
