use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::lock::OsLock;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "installed build directories are marked and removed only here"
    )]

    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub(super) fn open_lock(path: &Path) -> io::Result<File> {
        crate::platform::private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    pub(super) fn remove_dir_all(path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    pub(super) fn mkdir(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
}

const LOCK: &str = "in-use.lock";
const STARTED: &str = "started";
const PINNED: &str = "pinned";
const ORDER: &str = "order";
const ORDER_LOCK: &str = "order.lock";
const KEEP_IDLE: usize = 2;
const BUILD_TAG_HEX_BYTES: usize = 16;
const MAX_ORDER_RECORD_BYTES: usize = 32;

pub(crate) struct Admission {
    pub(crate) program: PathBuf,
    _usage: OsLock,
}

pub(crate) fn admit(source: &Path, program: PathBuf) -> io::Result<Admission> {
    admit_using(source, program, |_stage| Ok(()), |_published| Ok(()))
}

fn admit_using(
    source: &Path,
    program: PathBuf,
    before_selection: impl FnOnce(&Path) -> io::Result<()>,
    after_publication: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<Admission> {
    let directory = installed_directory(&program)
        .ok_or_else(|| io::Error::other("the admission target is not an installed build"))?;
    let versions = versions_of(&directory)?;
    raw::mkdir(versions).map_err(|error| admission_error("creating versions", error))?;
    let stage = tempfile::Builder::new()
        .prefix(".admission-")
        .tempdir_in(versions)
        .map_err(|error| admission_error("creating the private candidate", error))?;
    let stage = crate::platform::replacement::PrivateCandidate::new(stage)
        .map_err(|error| admission_error("protecting the private candidate", error))?;
    let staged = stage.path().join("bin").join(
        program
            .file_name()
            .ok_or_else(|| io::Error::other("the program has no filename"))?,
    );
    crate::platform::user_files::install_executable(source, &staged).map_err(io::Error::other)?;
    let usage_file = lock_file(stage.path())
        .map_err(|error| admission_error("opening the private usage lock", error))?;
    let staged_usage = OsLock::shared_file(usage_file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("holding the unpublished build: {error}"),
        )
    })?;
    before_selection(stage.path())?;
    let selection_file = raw::open_lock(&versions.join(ORDER_LOCK))
        .map_err(|error| admission_error("opening the selection lock", error))?;
    let _selection = OsLock::exclusive_file(selection_file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("selecting the published build: {error}"),
        )
    })?;
    match fs::symlink_metadata(&directory) {
        Ok(_) => {
            let existing = hold_published(&directory, &program)?;
            return Ok(Admission {
                program,
                _usage: existing,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(admission_error("inspecting the publication target", error)),
    }
    drop(staged_usage);
    crate::platform::replacement::publish_owned_directory(stage, &directory)?;
    after_publication(&directory)?;
    let usage = hold_published(&directory, &program)?;
    Ok(Admission {
        program,
        _usage: usage,
    })
}

fn check_owned(path: &Path, file: &File) -> io::Result<()> {
    match crate::platform::ownership(file)? {
        crate::platform::Ownership::Private | crate::platform::Ownership::Exposed(_) => Ok(()),
        crate::platform::Ownership::Foreign => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", path.display()),
        )),
    }
}

fn check_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || crate::platform::reparse_point(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not an ordinary build directory", path.display()),
        ));
    }
    let directory = crate::platform::open_private_dir(path)?;
    let opened = directory.metadata()?;
    if !opened.is_dir() || crate::platform::reparse_point(&opened) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the opened build directory changed its kind",
        ));
    }
    check_owned(path, &directory)
}

fn hold_published(directory: &Path, program: &Path) -> io::Result<OsLock> {
    check_directory(directory)?;
    let bin = program
        .parent()
        .ok_or_else(|| io::Error::other("the published program has no bin directory"))?;
    check_directory(bin)?;
    let metadata = fs::symlink_metadata(program)?;
    if !metadata.is_file() || crate::platform::reparse_point(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the published program is not an ordinary file",
        ));
    }
    let file = crate::platform::private_options()
        .read(true)
        .open(program)?;
    let opened = file.metadata()?;
    if !opened.is_file() || crate::platform::reparse_point(&opened) || opened.len() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the opened published program changed its kind",
        ));
    }
    check_owned(program, &file)?;
    let usage = crate::state_io::open_existing_lock(&directory.join(LOCK))
        .map_err(io::Error::other)?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the published usage lock is missing",
            )
        })?;
    OsLock::shared_file(usage)
}

#[derive(Debug, thiserror::Error)]
#[error("{action}: {source}")]
struct AdmissionError {
    action: &'static str,
    source: io::Error,
}

fn admission_error(action: &'static str, source: io::Error) -> io::Error {
    io::Error::new(source.kind(), AdmissionError { action, source })
}

fn installed_directory(executable: &Path) -> Option<PathBuf> {
    let bin = executable.parent()?;
    let build = bin.parent()?;
    let versions = build.parent()?;
    let bin_name = bin.file_name()?;
    (bin_name == "bin" && versions.file_name()? == "versions").then(|| build.to_path_buf())
}

fn versions_of(directory: &Path) -> io::Result<&Path> {
    directory
        .parent()
        .ok_or_else(|| io::Error::other("an installed build has no versions directory"))
}

fn lock_file(directory: &Path) -> io::Result<File> {
    raw::open_lock(&directory.join(LOCK))
}

pub(crate) fn hold_current() -> io::Result<Option<OsLock>> {
    hold_executable(std::env::current_exe(), OsLock::shared_file)
}

fn hold_executable(
    executable: io::Result<PathBuf>,
    lock_shared: impl FnOnce(File) -> io::Result<OsLock>,
) -> io::Result<Option<OsLock>> {
    let executable = fs::canonicalize(executable?)?;
    let Some(directory) = installed_directory(&executable) else {
        return Ok(None);
    };
    let versions = versions_of(&directory)?;
    let _selection = OsLock::exclusive_file(raw::open_lock(&versions.join(ORDER_LOCK))?)?;
    let guard = lock_shared(lock_file(&directory)?)?;
    let place = next_place_unlocked(versions, raw::write)?;
    raw::write(&directory.join(STARTED), place.to_string().as_bytes())?;
    Ok(Some(guard))
}

#[cfg(test)]
fn next_place(versions: &Path) -> io::Result<u64> {
    next_place_using(versions, OsLock::exclusive_file, raw::write)
}

#[cfg(test)]
fn next_place_using(
    versions: &Path,
    lock: impl FnOnce(File) -> io::Result<OsLock>,
    write: impl FnOnce(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<u64> {
    let order = lock(raw::open_lock(&versions.join(ORDER_LOCK))?)?;
    let next = next_place_unlocked(versions, write)?;
    order.release()?;
    Ok(next)
}

fn next_place_unlocked(
    versions: &Path,
    write: impl FnOnce(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<u64> {
    let next = number(&versions.join(ORDER))?
        .unwrap_or(0)
        .saturating_add(1);
    write(&versions.join(ORDER), next.to_string().as_bytes())?;
    Ok(next)
}

fn record(path: &Path) -> io::Result<Option<String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match crate::bounded::read(file, MAX_ORDER_RECORD_BYTES)?.map(String::from_utf8) {
        Some(Ok(text)) => Ok(Some(text.trim().to_owned())),
        Some(Err(_)) | None => Ok(None),
    }
}

fn number(path: &Path) -> io::Result<Option<u64>> {
    Ok(match record(path)?.map(|text| text.parse()) {
        Some(Ok(number)) => Some(number),
        Some(Err(_)) | None => None,
    })
}

#[cfg(test)]
fn running(directory: &Path) -> io::Result<bool> {
    match OsLock::try_file(lock_file(directory)?)? {
        Some(guard) => {
            guard.release()?;
            Ok(false)
        }
        None => Ok(true),
    }
}

pub(crate) fn pin(program: &Path) -> io::Result<()> {
    let directory = installed_directory(program).ok_or_else(|| {
        io::Error::other(format!("{} is not an installed build", program.display()))
    })?;
    let tag = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    raw::write(&versions_of(&directory)?.join(PINNED), tag.as_bytes())
}

pub(crate) fn prune(versions: &Path, keep: &str) -> io::Result<Vec<String>> {
    prune_entries(
        versions,
        keep,
        |directory| fs::read_dir(directory),
        (fs::DirEntry::file_type, raw::remove_dir_all),
    )
}

fn prune_entries<I>(
    versions: &Path,
    keep: &str,
    read_dir: impl FnOnce(&Path) -> io::Result<I>,
    (mut file_type, mut remove): (
        impl FnMut(&fs::DirEntry) -> io::Result<fs::FileType>,
        impl FnMut(&Path) -> io::Result<()>,
    ),
) -> io::Result<Vec<String>>
where
    I: IntoIterator<Item = io::Result<fs::DirEntry>>,
{
    let entries = match read_dir(versions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let _selection = OsLock::exclusive_file(raw::open_lock(&versions.join(ORDER_LOCK))?)?;
    let pinned = record(&versions.join(PINNED))?;
    let mut idle = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == keep
            || pinned.as_deref() == Some(name.as_str())
            || !file_type(&entry)?.is_dir()
            || !is_build_tag(&name)
        {
            continue;
        }
        let directory = entry.path();
        let Some(usage) = OsLock::try_file(lock_file(&directory)?)? else {
            continue;
        };
        idle.push((
            number(&directory.join(STARTED))?.unwrap_or(0),
            name,
            directory,
            usage,
        ));
    }
    let mut removed = Vec::new();
    let (idle, _idle_usage): (Vec<_>, Vec<_>) = idle
        .into_iter()
        .map(|(started, name, directory, usage)| ((started, name, directory), usage))
        .unzip();
    for (_, name, directory) in crate::retention::beyond_newest(idle, KEEP_IDLE) {
        match remove(&directory) {
            Ok(()) => removed.push(name),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }
    removed.sort();
    Ok(removed)
}

fn is_build_tag(name: &str) -> bool {
    name.len() == BUILD_TAG_HEX_BYTES && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{
        LOCK, ORDER, ORDER_LOCK, PINNED, STARTED, hold_executable, installed_directory,
        is_build_tag, lock_file, next_place, next_place_using, pin, prune, prune_entries, raw,
        running,
    };
    use crate::lock::OsLock;
    use crate::testing;
    use std::fs::{self, File};
    use std::io;
    use std::path::{Path, PathBuf};

    fn newer_idle_companions(versions: &Path) {
        for (tag, started) in [("2222222222222222", b"2"), ("3333333333333333", b"3")] {
            let directory = versions.join(tag);
            crate::state_io::private_dir(&directory).unwrap();
            testing::write(&directory.join(STARTED), started);
            crate::state_io::create_empty(&directory.join(LOCK)).unwrap();
        }
    }

    fn concurrent_prune(versions: &Path) {
        let mut command = crate::process::command(std::env::current_exe().unwrap());
        command
            .env("DOMYJOB_BUILD_ADMISSION_PRUNE", versions)
            .args(["--exact", "builds::tests::admission_pruner_process"]);
        let child = silent_fixture_process(command);
        assert!(child.wait().unwrap().success());
    }

    fn silent_fixture_process(command: std::process::Command) -> crate::process::Group {
        crate::process::Group::spawn_stdio(
            command,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
        )
        .unwrap()
    }

    fn assert_no_admission_stages(versions: &Path) {
        assert!(fs::read_dir(versions).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".admission-")
        }));
    }

    #[test]
    fn admission_pruner_process() {
        let Some(root) = crate::platform::variable("DOMYJOB_BUILD_ADMISSION_PRUNE") else {
            return;
        };
        if crate::platform::variable("DOMYJOB_BUILD_ADMISSION_BARRIER").is_some() {
            assert!(
                OsLock::try_file(raw::open_lock(&Path::new(&root).join(ORDER_LOCK)).unwrap())
                    .unwrap()
                    .is_none()
            );
            testing::signal(b"B");
        }
        prune(Path::new(&root), "fedcba9876543210").unwrap();
    }

    #[test]
    fn a_legacy_exposed_parent_cannot_expose_the_new_candidate_usage_lock() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        crate::platform::create_exposed_build_parent(&versions).unwrap();
        let original_parent = crate::platform::open_private_dir(&versions).unwrap();
        assert!(matches!(
            crate::platform::ownership(&original_parent).unwrap(),
            crate::platform::Ownership::Exposed(_)
        ));
        drop(original_parent);
        let program = versions.join("0123456789abcdef/bin/domyjob");
        let admission = super::admit(&std::env::current_exe().unwrap(), program.clone()).unwrap();
        let usage =
            crate::state_io::open_existing_lock(&versions.join("0123456789abcdef").join(LOCK))
                .unwrap()
                .unwrap();
        assert_eq!(
            crate::platform::ownership(&usage).unwrap(),
            crate::platform::Ownership::Private
        );
        drop(usage);
        let parent = crate::platform::open_private_dir(&versions).unwrap();
        assert!(matches!(
            crate::platform::ownership(&parent).unwrap(),
            crate::platform::Ownership::Exposed(_)
        ));
        assert_eq!(admission.program, program);
        drop(admission);
        assert_no_admission_stages(&versions);
    }

    #[test]
    fn selection_protects_a_published_build_until_final_usage_is_acquired() {
        use std::io::Read as _;
        const BARRIER_TIMEOUT_MILLIS: u64 = 10_000;
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        newer_idle_companions(&versions);
        let program = versions.join("0123456789abcdef/bin/domyjob");
        let mut pruner = None;
        let admitted = super::admit_using(
            &std::env::current_exe().unwrap(),
            program.clone(),
            |_stage| Ok(()),
            |published| {
                assert_eq!(
                    OsLock::probe(&published.join(LOCK)).unwrap(),
                    crate::lock::Probe::Free
                );
                let (mut reader, writer) = io::pipe()?;
                let mut command = crate::process::command(std::env::current_exe()?);
                command
                    .env("DOMYJOB_BUILD_ADMISSION_PRUNE", &versions)
                    .env("DOMYJOB_BUILD_ADMISSION_BARRIER", "1")
                    .args([
                        "--exact",
                        "builds::tests::admission_pruner_process",
                        "--nocapture",
                    ]);
                let child = crate::process::Group::spawn_stdio(
                    command,
                    writer.into(),
                    std::process::Stdio::inherit(),
                )
                .map_err(io::Error::other)?;
                let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let mut bytes = [0_u8; 1];
                    loop {
                        if let Err(error) = reader.read_exact(&mut bytes) {
                            let _sent = sender.send(Err(error));
                            return;
                        }
                        if bytes == *b"B" {
                            let _sent = sender.send(Ok(()));
                            let _drained = io::copy(&mut reader, &mut io::sink());
                            return;
                        }
                    }
                });
                match crate::platform::clock::receive(
                    &receiver,
                    crate::platform::clock::Deadline::after_millis(BARRIER_TIMEOUT_MILLIS),
                ) {
                    crate::platform::clock::Waited::Received(result) => result?,
                    crate::platform::clock::Waited::Expired
                    | crate::platform::clock::Waited::Closed => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "pruner did not observe the publication selection barrier",
                        ));
                    }
                }
                assert!(fs::metadata(&program)?.is_file());
                pruner = Some(child);
                Ok(())
            },
        )
        .unwrap();
        assert!(pruner.unwrap().wait().unwrap().success());
        assert!(fs::metadata(&program).unwrap().is_file());
        drop(admitted);
        concurrent_prune(&versions);
        assert_eq!(
            fs::metadata(program).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn an_unprotected_completed_install_is_removed_before_its_first_launch() {
        let (_root, program, directory) = installed_fixture();
        newer_idle_companions(directory.parent().unwrap());
        assert!(fs::metadata(&program).unwrap().is_file());
        concurrent_prune(directory.parent().unwrap());
        assert_eq!(
            fs::metadata(&program).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn published_candidate_is_owned_before_first_launch_without_changing_the_pin() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        newer_idle_companions(&versions);
        let program = versions.join("0123456789abcdef/bin/domyjob");
        testing::write(&versions.join(PINNED), b"1111111111111111");
        let admitted = super::admit(&std::env::current_exe().unwrap(), program.clone()).unwrap();
        concurrent_prune(&versions);
        assert!(fs::metadata(&program).unwrap().is_file());
        assert_eq!(testing::read(&versions.join(PINNED)), "1111111111111111");
        drop(admitted);
        concurrent_prune(&versions);
        assert_eq!(
            fs::metadata(&program).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_losing_publisher_holds_the_winner_before_discarding_its_own_stage() {
        const HEADER_BYTES: usize = 64;
        let root = tempfile::tempdir().unwrap();
        let source = std::env::current_exe().unwrap();
        let versions = root.path().join("versions");
        newer_idle_companions(&versions);
        let program = versions.join("0123456789abcdef/bin/domyjob");
        let replacement = root.path().join("replacement");
        testing::write(&versions.join(PINNED), b"1111111111111111");
        testing::write(
            &replacement,
            b"the losing executable must never be published",
        );
        let expected = crate::bounded::prefix(File::open(&source).unwrap(), HEADER_BYTES).unwrap();
        let mut winner = None;
        let mut unpublished = None;
        let loser = super::admit_using(
            &replacement,
            program.clone(),
            |stage| {
                unpublished = Some(stage.to_path_buf());
                winner = Some(super::admit(&source, program.clone())?);
                Ok(())
            },
            |_published| Ok(()),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(unpublished.unwrap()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(loser.program, program);
        assert_eq!(testing::read(&versions.join(PINNED)), "1111111111111111");
        assert_eq!(
            crate::bounded::prefix(File::open(&program).unwrap(), HEADER_BYTES).unwrap(),
            expected
        );
        drop(winner);
        concurrent_prune(&versions);
        assert!(fs::metadata(&program).unwrap().is_file());
        drop(loser);
        concurrent_prune(&versions);
        assert_eq!(
            fs::metadata(program).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum InvalidPublication {
        BuildFile,
        BinFile,
        ProgramDirectory,
        EmptyProgram,
        MissingUsage,
        UsageDirectory,
    }

    fn corrupt_publication(program: &Path, kind: InvalidPublication) {
        let directory = program.parent().unwrap().parent().unwrap();
        match kind {
            InvalidPublication::BuildFile => testing::write(directory, b"not a directory"),
            InvalidPublication::BinFile => {
                testing::write(program.parent().unwrap(), b"not a directory");
            }
            InvalidPublication::ProgramDirectory => testing::mkdir(program),
            InvalidPublication::EmptyProgram => testing::write(program, b""),
            InvalidPublication::MissingUsage => testing::write(program, b"published executable"),
            InvalidPublication::UsageDirectory => {
                testing::write(program, b"published executable");
                testing::mkdir(&directory.join(LOCK));
            }
        }
    }

    #[test]
    fn invalid_existing_publications_are_refused_without_repair_or_new_usage_locks() {
        for kind in [
            InvalidPublication::BuildFile,
            InvalidPublication::BinFile,
            InvalidPublication::ProgramDirectory,
            InvalidPublication::EmptyProgram,
            InvalidPublication::MissingUsage,
            InvalidPublication::UsageDirectory,
        ] {
            let root = tempfile::tempdir().unwrap();
            let versions = root.path().join("versions");
            let program = versions.join("0123456789abcdef/bin/domyjob");
            corrupt_publication(&program, kind);
            let error = match super::admit(&std::env::current_exe().unwrap(), program.clone()) {
                Ok(_) => panic!("corrupt existing publications cannot be admitted"),
                Err(error) => error,
            };
            let expected = match kind {
                InvalidPublication::BuildFile
                | InvalidPublication::BinFile
                | InvalidPublication::ProgramDirectory
                | InvalidPublication::EmptyProgram => io::ErrorKind::InvalidData,
                InvalidPublication::MissingUsage => io::ErrorKind::NotFound,
                InvalidPublication::UsageDirectory => io::ErrorKind::Other,
            };
            assert_eq!(error.kind(), expected, "{kind:?}: {error}");
            assert_no_admission_stages(&versions);
            if matches!(kind, InvalidPublication::MissingUsage) {
                assert_eq!(
                    fs::metadata(program.parent().unwrap().parent().unwrap().join(LOCK))
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::NotFound
                );
                assert_eq!(testing::read(&program), "published executable");
            }
        }
    }

    #[test]
    fn linked_existing_publications_are_refused_without_following_or_changing_the_target() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        let directory = versions.join("0123456789abcdef");
        let target = root.path().join("target");
        testing::mkdir(&versions);
        testing::write(&target.join("bin/domyjob"), b"keep linked executable");
        crate::file_kind::directory_link(&target, &directory).unwrap();
        let error = match super::admit(
            &std::env::current_exe().unwrap(),
            directory.join("bin/domyjob"),
        ) {
            Ok(_) => panic!("links cannot be published build directories"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            testing::read(&target.join("bin/domyjob")),
            "keep linked executable"
        );
        assert_eq!(
            fs::metadata(target.join(LOCK)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    fn installed_program(root: &Path) -> PathBuf {
        let program = root
            .join("versions/0123456789abcdef/bin")
            .join(format!("domyjob{}", std::env::consts::EXE_SUFFIX));
        testing::mkdir(program.parent().unwrap());
        testing::write(&program, b"installed executable");
        program
    }

    fn installed_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let program = installed_program(root.path());
        let directory = installed_directory(&program).unwrap();
        (root, program, directory)
    }

    fn assert_build_held(directory: &Path, held: OsLock) {
        assert!(running(directory).unwrap());
        assert_eq!(testing::read(&directory.join(STARTED)), "1");
        drop(held);
        assert!(!running(directory).unwrap());
    }

    fn assert_not_started(directory: &Path) {
        assert_eq!(
            File::open(directory.join(STARTED)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn opening_an_existing_lock_preserves_readable_contents() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(LOCK);
        testing::write(&path, b"existing lock contents");
        let file = raw::open_lock(&path).unwrap();
        let contents = crate::bounded::read(file, 64).unwrap().unwrap();
        assert_eq!(contents, b"existing lock contents");
        assert_eq!(testing::read(&path), "existing lock contents");
    }

    #[test]
    fn installed_directory_rejects_short_paths_and_partial_layouts() {
        for program in [
            "",
            "domyjob",
            "bin/domyjob",
            "outer/build/../domyjob",
            "/build/bin/domyjob",
            "/versions/build/lib/domyjob",
            "/other/build/bin/domyjob",
        ] {
            assert_eq!(installed_directory(Path::new(program)), None, "{program}");
        }
    }

    #[test]
    fn an_installed_start_retains_its_lock_and_records_its_order() {
        let (_root, program, directory) = installed_fixture();
        let held = hold_executable(Ok(program), OsLock::shared_file)
            .unwrap()
            .unwrap();
        assert_build_held(&directory, held);
    }

    #[test]
    fn an_installed_start_propagates_executable_lookup_and_canonicalization_errors() {
        let lookup_error =
            hold_executable(Err(io::Error::other("executable lookup failed")), |_| {
                panic!("lookup failure must precede locking")
            })
            .unwrap_err();
        assert_eq!(lookup_error.to_string(), "executable lookup failed");
        let root = tempfile::tempdir().unwrap();
        let canonicalization_error = hold_executable(Ok(root.path().join("missing")), |_| {
            panic!("canonicalization failure must precede locking")
        })
        .unwrap_err();
        assert_eq!(canonicalization_error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn an_installed_start_propagates_lock_open_errors() {
        let (_root, program, directory) = installed_fixture();
        testing::mkdir(&directory.join(LOCK));
        hold_executable(Ok(program), |_| {
            panic!("lock opening failure must precede locking")
        })
        .unwrap_err();
    }

    #[test]
    fn an_installed_start_propagates_shared_lock_errors() {
        let (_root, program, directory) = installed_fixture();
        let error = hold_executable(Ok(program), |_| Err(io::Error::other("shared lock failed")))
            .unwrap_err();
        assert_eq!(error.to_string(), "shared lock failed");
        assert_not_started(&directory);
    }

    #[test]
    fn an_installed_start_propagates_order_errors() {
        let (_root, program, directory) = installed_fixture();
        testing::mkdir(&directory.parent().unwrap().join(ORDER_LOCK));
        hold_executable(Ok(program), OsLock::shared_file).unwrap_err();
        assert_not_started(&directory);
    }

    #[test]
    fn an_installed_start_propagates_started_record_errors() {
        let (_root, program, directory) = installed_fixture();
        testing::mkdir(&directory.join(STARTED));
        hold_executable(Ok(program), OsLock::shared_file).unwrap_err();
        assert_eq!(testing::read(&directory.parent().unwrap().join(ORDER)), "1");
    }

    #[test]
    fn starting_order_propagates_lock_open_errors() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        testing::mkdir(&versions.join(ORDER_LOCK));
        next_place(&versions).unwrap_err();
    }

    #[test]
    fn starting_order_propagates_lock_errors() {
        let root = tempfile::tempdir().unwrap();
        let error = next_place_using(
            root.path(),
            |_| Err(io::Error::other("order lock failed")),
            |_, _| panic!("lock failure must precede writing"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "order lock failed");
    }

    #[test]
    fn starting_order_propagates_record_read_errors() {
        let root = tempfile::tempdir().unwrap();
        testing::mkdir(&root.path().join(ORDER));
        next_place(root.path()).unwrap_err();
    }

    #[test]
    fn starting_order_propagates_record_write_errors() {
        let root = tempfile::tempdir().unwrap();
        testing::write(&root.path().join(ORDER), b"4");
        let error = next_place_using(root.path(), OsLock::exclusive_file, |path, bytes| {
            assert_eq!(path, root.path().join(ORDER));
            assert_eq!(bytes, b"5");
            Err(io::Error::other("order record write failed"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "order record write failed");
        assert_eq!(testing::read(&root.path().join(ORDER)), "4");
    }

    #[test]
    fn checking_a_running_build_propagates_lock_open_errors() {
        let root = tempfile::tempdir().unwrap();
        testing::mkdir(&root.path().join(LOCK));
        running(root.path()).unwrap_err();
    }

    #[test]
    fn pinning_requires_an_installed_build_path() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("domyjob");
        let error = pin(&program).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("{} is not an installed build", program.display())
        );
    }

    #[test]
    fn build_tags_require_exactly_sixteen_ascii_hex_digits() {
        assert!(is_build_tag("0123456789abcdef"));
        assert!(is_build_tag("0123456789ABCDEF"));
        for name in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "gggggggggggggggg",
            "0123456789abcdé",
        ] {
            assert!(!is_build_tag(name), "{name}");
        }
    }

    #[test]
    fn starting_an_installed_process_marks_and_holds_its_build() {
        let executable = std::env::current_exe().unwrap();
        let bytes = crate::bounded::read(File::open(&executable).unwrap(), 64 * 1024 * 1024)
            .unwrap()
            .expect("the unit executable fits the fixture bound");
        let (_root, program, directory) = installed_fixture();
        testing::write(&program, bytes);
        testing::restore(&program, fs::metadata(&executable).unwrap().permissions());
        let mut command = crate::process::command(&program);
        command
            .env_clear()
            .env("DOMYJOB_INSTALLED_BUILD_TEST", "1")
            .arg("--exact")
            .arg("builds::tests::current_process_marks_its_installed_build");
        let child = silent_fixture_process(command);
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "installed unit process exited with {status}"
        );
        assert_eq!(testing::read(&directory.join(STARTED)), "1");
        assert_eq!(testing::read(&directory.parent().unwrap().join(ORDER)), "1");
        assert!(!running(&directory).unwrap());
    }

    #[test]
    fn current_process_marks_its_installed_build() {
        if crate::platform::variable("DOMYJOB_INSTALLED_BUILD_TEST").is_none() {
            return;
        }
        let executable = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let directory = installed_directory(&executable).unwrap();
        let held = super::hold_current().unwrap().unwrap();
        assert_build_held(&directory, held);
    }

    #[test]
    fn pruning_accepts_a_missing_tree_and_propagates_other_listing_errors() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        assert_eq!(prune(&missing, "keep").unwrap(), Vec::<String>::new());
        let file = root.path().join("file");
        testing::write(&file, b"a file cannot contain installed builds");
        prune(&file, "keep").unwrap_err();
    }

    #[test]
    fn pruning_propagates_pinned_record_read_errors() {
        let root = tempfile::tempdir().unwrap();
        testing::mkdir(&root.path().join(PINNED));
        prune(root.path(), "keep").unwrap_err();
    }

    #[test]
    fn pruning_propagates_directory_iteration_errors() {
        let root = tempfile::tempdir().unwrap();
        let error = prune_entries(
            root.path(),
            "keep",
            |_| {
                Ok(std::iter::once(Err(io::Error::other(
                    "build iteration failed",
                ))))
            },
            (fs::DirEntry::file_type, raw::remove_dir_all),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "build iteration failed");
    }

    #[test]
    fn pruning_propagates_file_type_errors() {
        let root = tempfile::tempdir().unwrap();
        testing::mkdir(&root.path().join("0123456789abcdef"));
        let error = prune_entries(
            root.path(),
            "keep",
            |directory| fs::read_dir(directory),
            (
                |_| Err(io::Error::other("build file type failed")),
                raw::remove_dir_all,
            ),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "build file type failed");
    }

    #[test]
    fn pruning_propagates_running_lock_errors() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("0123456789abcdef");
        testing::mkdir(&directory.join(LOCK));
        prune(root.path(), "keep").unwrap_err();
        assert!(testing::is_dir(&directory));
    }

    #[test]
    fn pruning_propagates_started_record_read_errors() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("0123456789abcdef");
        testing::mkdir(&directory.join(STARTED));
        prune(root.path(), "keep").unwrap_err();
        assert!(testing::is_dir(&directory));
    }

    #[test]
    fn pruning_keeps_inaccessible_old_builds_and_propagates_other_removal_errors() {
        let root = tempfile::tempdir().unwrap();
        for tag in 1..=3 {
            let directory = root.path().join(format!("{tag:016}"));
            testing::mkdir(&directory);
            testing::write(&directory.join(STARTED), tag.to_string());
        }
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other] {
            let result = prune_entries(
                root.path(),
                "keep",
                |directory| fs::read_dir(directory),
                (fs::DirEntry::file_type, |directory| {
                    assert_eq!(directory, root.path().join("0000000000000001"));
                    Err(io::Error::new(kind, "build removal failed"))
                }),
            );
            if kind == io::ErrorKind::PermissionDenied {
                assert_eq!(result.unwrap().len(), 0);
            } else {
                assert_eq!(result.unwrap_err().to_string(), "build removal failed");
            }
            for tag in 1..=3 {
                assert!(testing::is_dir(&root.path().join(format!("{tag:016}"))));
            }
        }
    }

    #[test]
    fn builds_stay_while_current_running_pinned_or_among_the_last_started() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        for (tag, started) in [
            (1, Some(1)),
            (2, Some(2)),
            (3, Some(3)),
            (4, Some(4)),
            (5, Some(5)),
            (6, None),
            (7, Some(7)),
        ] {
            let directory = versions.join(format!("{tag:016}"));
            testing::mkdir(&directory.join("bin"));
            if let Some(place) = started {
                testing::write(&directory.join(STARTED), format!("{place}"));
            }
        }
        testing::write(&versions.join(PINNED), format!("{:016}", 4));
        testing::mkdir(&versions.join("notes"));
        let running =
            OsLock::shared_file(lock_file(&versions.join(format!("{:016}", 2))).unwrap()).unwrap();
        assert_eq!(
            prune(&versions, &format!("{:016}", 7)).unwrap(),
            [format!("{:016}", 1), format!("{:016}", 6)]
        );
        for kept in [2, 3, 4, 5, 7] {
            assert!(testing::is_dir(&versions.join(format!("{kept:016}"))));
        }
        assert!(testing::is_dir(&versions.join("notes")));
        drop(running);
        assert_eq!(
            prune(&versions, &format!("{:016}", 7)).unwrap(),
            [format!("{:016}", 2)]
        );
    }

    #[test]
    fn each_start_takes_the_next_place_and_setup_pins_one_build() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        testing::mkdir(&versions);
        assert_eq!(next_place(&versions).unwrap(), 1);
        assert_eq!(next_place(&versions).unwrap(), 2);
        let program = |tag: u64| {
            versions
                .join(format!("{tag:016}"))
                .join("bin")
                .join("domyjob")
        };
        for tag in [1, 2] {
            testing::mkdir(&versions.join(format!("{tag:016}")).join("bin"));
        }
        pin(&program(1)).unwrap();
        pin(&program(2)).unwrap();
        assert_eq!(testing::read(&versions.join(PINNED)), format!("{:016}", 2));
    }

    #[test]
    fn only_executables_inside_a_versions_tree_belong_to_a_build() {
        let installed = Path::new("/home/me/.cargo/domyjob/versions/00ff/bin/domyjob");
        assert_eq!(
            installed_directory(installed).as_deref(),
            Some(Path::new("/home/me/.cargo/domyjob/versions/00ff"))
        );
        assert_eq!(
            installed_directory(Path::new("/repo/target/debug/domyjob")),
            None
        );
    }
}
