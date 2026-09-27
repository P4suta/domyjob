use std::path::Path;

#[cfg(any(test, feature = "failpoints"))]
#[must_use]
pub fn from_environment() -> Option<&'static mut fail::FailScenario<'static>> {
    std::env::var_os("FAILPOINTS")
        .is_some()
        .then(|| Box::leak(Box::new(fail::FailScenario::setup())))
}

#[cfg(not(any(test, feature = "failpoints")))]
#[must_use]
pub const fn from_environment() -> Option<()> {
    None
}

#[cfg(any(test, feature = "failpoints"))]
const FULL: &str = "full:";

pub fn at(site: &'static str, path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    crashing(path)?;
    #[cfg(test)]
    injected(site, path)?;
    #[cfg(feature = "failpoints")]
    if std::env::var_os("DOMYJOB_ABORT_AT").is_some_and(|wanted| wanted == site) {
        std::process::abort();
    }
    #[cfg(any(test, feature = "failpoints"))]
    let hit = fail::eval(site, |tag| {
        tag.map(|tag| {
            let (kind, wanted) = match tag.strip_prefix(FULL) {
                Some(wanted) => (std::io::ErrorKind::StorageFull, wanted.to_owned()),
                None => (std::io::ErrorKind::Other, tag),
            };
            (kind, path.as_os_str().to_string_lossy().contains(&wanted))
        })
    });
    #[cfg(not(any(test, feature = "failpoints")))]
    let hit: Option<Option<(std::io::ErrorKind, bool)>> = None;
    match hit {
        Some(Some((kind, true))) => Err(std::io::Error::new(
            kind,
            format!("a fault injected at {site} for {}", path.display()),
        )),
        Some(Some((_, false)) | None) | None => Ok(()),
    }
}

#[cfg(test)]
enum Scope {
    Contains(String),
    Within(std::path::PathBuf),
}

#[cfg(test)]
struct Rule {
    site: String,
    scope: Scope,
    kind: std::io::ErrorKind,
    passes: usize,
}

#[cfg(test)]
struct Injection {
    id: u64,
    rules: Vec<Rule>,
}

#[cfg(test)]
static INJECTIONS: std::sync::Mutex<Vec<Injection>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
static NEXT_FAULT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
fn next_fault_id() -> u64 {
    NEXT_FAULT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
fn injected(site: &str, path: &Path) -> std::io::Result<()> {
    let mut held = INJECTIONS.lock().map_err(|poison| {
        std::io::Error::other(format!("the test fault registry is poisoned: {poison}"))
    })?;
    for injection in held.iter_mut() {
        for rule in &mut injection.rules {
            let matches = match &rule.scope {
                Scope::Contains(tag) => path.as_os_str().to_string_lossy().contains(tag),
                Scope::Within(root) => path.starts_with(root),
            };
            if rule.site != site || !matches {
                continue;
            }
            if rule.passes > 0 {
                rule.passes = rule.passes.saturating_sub(1);
            } else {
                return Err(std::io::Error::new(
                    rule.kind,
                    format!("a fault injected at {site} for {}", path.display()),
                ));
            }
        }
    }
    drop(held);
    Ok(())
}

#[cfg(test)]
#[derive(Debug)]
struct Crash {
    id: u64,
    within: std::path::PathBuf,
    survives: Option<usize>,
    taken: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
static CRASH: std::sync::Mutex<Vec<Crash>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn crashing(path: &Path) -> std::io::Result<()> {
    let mut held = CRASH.lock().map_err(|poison| {
        std::io::Error::other(format!("the test crash registry is poisoned: {poison}"))
    })?;
    for crash in held
        .iter_mut()
        .filter(|crash| path.starts_with(&crash.within))
    {
        crash
            .taken
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match &mut crash.survives {
            Some(0) => {
                return Err(std::io::Error::other(format!(
                    "the machine crashed before this step on {}",
                    path.display()
                )));
            }
            Some(left) => *left = left.saturating_sub(1),
            None => {}
        }
    }
    drop(held);
    Ok(())
}

#[cfg(test)]
pub struct Crashing {
    id: u64,
    taken: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl std::fmt::Debug for Crashing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Crashing").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl Crashing {
    #[must_use]
    pub fn steps(&self) -> usize {
        self.taken.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl Drop for Crashing {
    fn drop(&mut self) {
        CRASH.lock().unwrap().retain(|crash| crash.id != self.id);
    }
}

#[cfg(test)]
#[must_use]
pub fn crash_after(within: &Path, survives: Option<usize>) -> Crashing {
    assert!(within.is_absolute(), "a crash needs an absolute path scope");
    let id = next_fault_id();
    let taken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    CRASH.lock().unwrap().push(Crash {
        id,
        within: within.to_path_buf(),
        survives,
        taken: std::sync::Arc::clone(&taken),
    });
    Crashing { id, taken }
}

#[cfg(test)]
pub struct Injected {
    id: u64,
}

#[cfg(test)]
impl std::fmt::Debug for Injected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injected").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl Drop for Injected {
    fn drop(&mut self) {
        INJECTIONS
            .lock()
            .unwrap()
            .retain(|injection| injection.id != self.id);
    }
}

#[cfg(test)]
#[must_use]
pub fn inject(sites: &[(&str, &str)]) -> Injected {
    let id = next_fault_id();
    let rules = sites
        .iter()
        .map(|(site, tag)| {
            let (kind, wanted) = match tag.strip_prefix(FULL) {
                Some(wanted) => (std::io::ErrorKind::StorageFull, wanted),
                None => (std::io::ErrorKind::Other, *tag),
            };
            assert!(!wanted.is_empty(), "a test fault needs a path scope");
            Rule {
                site: (*site).to_owned(),
                scope: Scope::Contains(wanted.to_owned()),
                kind,
                passes: 0,
            }
        })
        .collect();
    INJECTIONS.lock().unwrap().push(Injection { id, rules });
    Injected { id }
}

#[cfg(test)]
#[must_use]
pub fn inject_after(site: &str, passes: usize, within: &Path) -> Injected {
    assert!(
        within.is_absolute(),
        "a delayed fault needs an absolute path scope"
    );
    let id = next_fault_id();
    INJECTIONS.lock().unwrap().push(Injection {
        id,
        rules: vec![Rule {
            site: site.to_owned(),
            scope: Scope::Within(within.to_path_buf()),
            kind: std::io::ErrorKind::Other,
            passes,
        }],
    });
    Injected { id }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fault_hits_only_the_paths_it_names() {
        let _faults = inject(&[("faults::probe", "doomed"), ("faults::full", "full:doomed")]);
        at("faults::probe", Path::new("/state/doomed/file")).unwrap_err();
        at("faults::probe", Path::new("/state/spared/file")).unwrap();
        at("faults::elsewhere", Path::new("/state/doomed/file")).unwrap();
        assert_eq!(
            at("faults::full", Path::new("/state/doomed/file"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::StorageFull
        );
        at("faults::full", Path::new("/state/spared/file")).unwrap();
    }

    #[test]
    fn a_later_fault_lets_the_first_calls_through() {
        let temp = tempfile::tempdir().unwrap();
        let doomed = temp.path().join("doomed");
        let spared = temp.path().join("spared");
        let _faults = inject_after("faults::later", 2, &doomed);
        at("faults::later", &doomed.join("a")).unwrap();
        at("faults::later", &spared.join("b")).unwrap();
        at("faults::later", &doomed.join("c")).unwrap();
        at("faults::later", &doomed.join("d")).unwrap_err();
        at("faults::later", &spared.join("d")).unwrap();
    }

    #[test]
    fn simultaneous_faults_are_confined_to_their_paths() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("first");
        let second_path = temp.path().join("second");
        let unrelated = temp.path().join("unrelated");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let _fault = inject_after("faults::parallel", 0, &first_path);
                barrier.wait();
                at("faults::parallel", &unrelated).unwrap();
                at("faults::parallel", &first_path.join("file")).unwrap_err();
            });
            let second = scope.spawn(|| {
                let _fault = inject_after("faults::parallel", 0, &second_path);
                barrier.wait();
                at("faults::parallel", &unrelated).unwrap();
                at("faults::parallel", &second_path.join("file")).unwrap_err();
            });
            first.join().unwrap();
            second.join().unwrap();
        });
    }

    #[test]
    fn simultaneous_crash_guards_count_only_their_paths() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("first");
        let second_path = temp.path().join("second");
        let first = crash_after(&first_path, None);
        let second = crash_after(&second_path, None);
        at("faults::parallel", &first_path.join("file")).unwrap();
        at("faults::parallel", &second_path.join("file")).unwrap();
        at("faults::parallel", &temp.path().join("unrelated")).unwrap();
        assert_eq!(first.steps(), 1);
        assert_eq!(second.steps(), 1);
    }
}

#[cfg(test)]
#[derive(Debug)]
pub struct Told(pub std::sync::mpsc::Sender<Vec<u8>>);

#[cfg(test)]
impl std::io::Write for Told {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.0.send(bytes.to_vec()) {
            Ok(()) | Err(_) => {}
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
