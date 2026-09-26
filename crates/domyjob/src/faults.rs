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
#[derive(Debug)]
struct Crash {
    within: std::path::PathBuf,
    survives: Option<usize>,
    taken: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
static CRASH: std::sync::Mutex<Option<Crash>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn crashing(path: &Path) -> std::io::Result<()> {
    let Ok(mut held) = CRASH.lock() else {
        return Ok(());
    };
    let Some(crash) = held
        .as_mut()
        .filter(|crash| path.starts_with(&crash.within))
    else {
        return Ok(());
    };
    crash
        .taken
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    match &mut crash.survives {
        Some(0) => Err(std::io::Error::other(format!(
            "the machine crashed before this step on {}",
            path.display()
        ))),
        Some(left) => {
            *left = left.saturating_sub(1);
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
pub struct Crashing {
    _scenario: fail::FailScenario<'static>,
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
        if let Ok(mut held) = CRASH.lock() {
            *held = None;
        }
    }
}

#[cfg(test)]
#[must_use]
pub fn crash_after(within: &Path, survives: Option<usize>) -> Crashing {
    let scenario = fail::FailScenario::setup();
    let taken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    if let Ok(mut held) = CRASH.lock() {
        *held = Some(Crash {
            within: within.to_path_buf(),
            survives,
            taken: std::sync::Arc::clone(&taken),
        });
    }
    Crashing {
        _scenario: scenario,
        taken,
    }
}

#[cfg(test)]
pub struct Injected {
    _scenario: fail::FailScenario<'static>,
}

#[cfg(test)]
impl std::fmt::Debug for Injected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injected").finish_non_exhaustive()
    }
}

#[cfg(test)]
#[must_use]
pub fn inject(sites: &[(&str, &str)]) -> Injected {
    let scenario = fail::FailScenario::setup();
    for (site, tag) in sites {
        fail::cfg(*site, &format!("return({tag})")).unwrap();
    }
    Injected {
        _scenario: scenario,
    }
}

#[cfg(test)]
#[must_use]
pub fn inject_after(site: &str, passes: usize, tag: &str) -> Injected {
    let scenario = fail::FailScenario::setup();
    fail::cfg(site, &format!("{passes}*off->return({tag})")).unwrap();
    Injected {
        _scenario: scenario,
    }
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
        let _faults = inject_after("faults::later", 2, "doomed");
        at("faults::later", Path::new("/state/doomed/a")).unwrap();
        at("faults::later", Path::new("/state/spared/b")).unwrap();
        at("faults::later", Path::new("/state/doomed/c")).unwrap_err();
        at("faults::later", Path::new("/state/spared/d")).unwrap();
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
