use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::lock::OsLock;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "installed build directories are marked and removed only here"
    )]

    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    pub(super) fn open_lock(path: &Path) -> io::Result<File> {
        OpenOptions::new()
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
}

const LOCK: &str = "in-use.lock";
const STARTED: &str = "started";
const PINNED: &str = "pinned";
const ORDER: &str = "order";
const ORDER_LOCK: &str = "order.lock";
const KEEP_IDLE: usize = 2;

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
    let guard = lock_shared(lock_file(&directory)?)?;
    let versions = versions_of(&directory)?;
    let place = next_place(versions)?;
    raw::write(&directory.join(STARTED), place.to_string().as_bytes())?;
    Ok(Some(guard))
}

fn next_place(versions: &Path) -> io::Result<u64> {
    next_place_using(versions, OsLock::exclusive_file, raw::write)
}

fn next_place_using(
    versions: &Path,
    lock: impl FnOnce(File) -> io::Result<OsLock>,
    write: impl FnOnce(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<u64> {
    let order = lock(raw::open_lock(&versions.join(ORDER_LOCK))?)?;
    let next = number(&versions.join(ORDER))?
        .unwrap_or(0)
        .saturating_add(1);
    write(&versions.join(ORDER), next.to_string().as_bytes())?;
    order.release()?;
    Ok(next)
}

fn record(path: &Path) -> io::Result<Option<String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match crate::bounded::read(file, 32)?.map(String::from_utf8) {
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
        if running(&directory)? {
            continue;
        }
        idle.push((
            number(&directory.join(STARTED))?.unwrap_or(0),
            name,
            directory,
        ));
    }
    let mut removed = Vec::new();
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
    name.len() == 16 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
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
        let child = crate::process::Group::spawn_stdio(
            command,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
        )
        .unwrap();
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
