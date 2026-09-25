use std::path::Path;

#[must_use]
pub fn from_environment() -> Option<&'static mut fail::FailScenario<'static>> {
    std::env::var_os("FAILPOINTS")
        .is_some()
        .then(|| Box::leak(Box::new(fail::FailScenario::setup())))
}

const FULL: &str = "full:";

pub fn at(site: &'static str, path: &Path) -> std::io::Result<()> {
    let hit = fail::eval(site, |tag| {
        tag.map(|tag| {
            let (kind, wanted) = match tag.strip_prefix(FULL) {
                Some(wanted) => (std::io::ErrorKind::StorageFull, wanted.to_owned()),
                None => (std::io::ErrorKind::Other, tag),
            };
            (kind, path.as_os_str().to_string_lossy().contains(&wanted))
        })
    });
    match hit {
        Some(Some((kind, true))) => Err(std::io::Error::new(
            kind,
            format!("a fault injected at {site} for {}", path.display()),
        )),
        Some(Some((_, false)) | None) | None => Ok(()),
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
