use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct LocalDirectory<'a>(&'a Path);

impl LocalDirectory<'_> {
    #[must_use]
    pub const fn path(&self) -> &Path {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct ServiceLog(PathBuf);

impl ServiceLog {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    home: PathBuf,
    state: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    pub keys: crate::keystore::KeyStore,
}

fn var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

impl Dirs {
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }

    #[must_use]
    pub fn config(&self) -> &Path {
        &self.config
    }

    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    #[must_use]
    pub(crate) fn with_supervisor_paths(
        mut self,
        state: Option<&PathBuf>,
        home: Option<&PathBuf>,
    ) -> Self {
        if let Some(state) = state {
            self.state.clone_from(state);
        }
        if let Some(home) = home {
            self.home.clone_from(home);
        }
        self
    }

    #[must_use]
    pub fn home_path(&self) -> LocalDirectory<'_> {
        LocalDirectory(&self.home)
    }

    #[must_use]
    pub fn state_path(&self) -> LocalDirectory<'_> {
        LocalDirectory(&self.state)
    }

    #[must_use]
    pub fn cache_path(&self) -> LocalDirectory<'_> {
        LocalDirectory(&self.cache)
    }

    #[must_use]
    pub fn service_log(&self) -> ServiceLog {
        ServiceLog(self.state.join("serve.log"))
    }

    #[must_use]
    pub fn from_env() -> Self {
        Self::of(crate::platform::FAMILY, &var)
    }

    #[must_use]
    fn of(family: Family, var: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let home = var("HOME")
            .or_else(|| var("USERPROFILE"))
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        let pick = |own: &str, xdg: &str, fallback: &[&str], windows: &str, windows_sub: &str| {
            if let Some(explicit) = var(own) {
                return PathBuf::from(explicit);
            }
            if family == Family::Windows
                && let Some(base) = var(windows)
            {
                return PathBuf::from(base).join("domyjob").join(windows_sub);
            }
            if let Some(base) = var(xdg) {
                return PathBuf::from(base).join("domyjob");
            }
            fallback
                .iter()
                .fold(home.clone(), |path, part| path.join(part))
                .join("domyjob")
        };
        Self {
            keys: crate::keystore::KeyStore::Platform,
            state: pick(
                "DOMYJOB_STATE",
                "XDG_STATE_HOME",
                &[".local", "state"],
                "LOCALAPPDATA",
                "state",
            ),
            config: pick(
                "DOMYJOB_CONFIG",
                "XDG_CONFIG_HOME",
                &[".config"],
                "APPDATA",
                "config",
            ),
            cache: pick(
                "DOMYJOB_CACHE",
                "XDG_CACHE_HOME",
                &[".cache"],
                "LOCALAPPDATA",
                "cache",
            ),
            home,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_test(root: &Path) -> Self {
        Self {
            home: root.into(),
            state: root.join("state"),
            config: root.join("c"),
            cache: root.join("k"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_test_state(mut self, state: PathBuf) -> Self {
        self.state = state;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_test_cache(mut self, cache: PathBuf) -> Self {
        self.cache = cache;
        self
    }

    #[cfg(any(test, feature = "failpoints"))]
    #[must_use]
    pub fn isolated_for_test(root: &Path) -> Self {
        Self {
            home: root.join("home"),
            state: root.join("state"),
            config: root.join("config"),
            cache: root.join("cache"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Unix,
    Windows,
}

#[derive(Clone, Copy)]
enum Availability {
    Available,
    Unavailable,
}

impl Availability {
    const fn enabled(self) -> bool {
        match self {
            Self::Available => true,
            Self::Unavailable => false,
        }
    }
}

struct FamilyCapabilities {
    links: Availability,
    modes: Availability,
    load_average: Availability,
    agent_socket: Availability,
    replaces_running_executables: Availability,
}

impl Family {
    const fn capabilities(self) -> FamilyCapabilities {
        match self {
            Self::Unix => FamilyCapabilities {
                links: Availability::Available,
                modes: Availability::Available,
                load_average: Availability::Available,
                agent_socket: Availability::Available,
                replaces_running_executables: Availability::Available,
            },
            Self::Windows => FamilyCapabilities {
                links: Availability::Unavailable,
                modes: Availability::Unavailable,
                load_average: Availability::Unavailable,
                agent_socket: Availability::Unavailable,
                replaces_running_executables: Availability::Unavailable,
            },
        }
    }

    #[must_use]
    pub const fn links(self) -> bool {
        self.capabilities().links.enabled()
    }

    #[must_use]
    pub const fn modes(self) -> bool {
        self.capabilities().modes.enabled()
    }

    #[must_use]
    pub const fn load_average(self) -> bool {
        self.capabilities().load_average.enabled()
    }

    #[must_use]
    pub const fn agent_socket(self) -> bool {
        self.capabilities().agent_socket.enabled()
    }

    #[must_use]
    pub const fn replaces_running_executables(self) -> bool {
        self.capabilities().replaces_running_executables.enabled()
    }

    #[must_use]
    pub fn remote_bin(self) -> crate::template::Arg {
        let key = crate::protocol::build_key();
        match self {
            Self::Unix => crate::template::Arg::joined(&[".cache/domyjob/bin/domyjob-", key]),
            Self::Windows => {
                crate::template::Arg::joined(&[".cache/domyjob/bin/domyjob-", key, ".exe"])
            }
        }
    }

    #[must_use]
    pub fn invoke(self) -> crate::template::Arg {
        let key = crate::protocol::build_key();
        match self {
            Self::Unix => {
                crate::template::Arg::joined(&["./.cache/domyjob/bin/domyjob-", key, " node"])
            }
            Self::Windows => crate::template::Arg::joined(&[
                ".\\.cache\\domyjob\\bin\\domyjob-",
                key,
                ".exe node",
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_family_finds_its_own_places_whatever_system_runs_the_test() {
        let set = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let windows = Dirs::of(
            Family::Windows,
            &set(&[
                ("USERPROFILE", "C:/Users/me"),
                ("LOCALAPPDATA", "C:/Users/me/AppData/Local"),
                ("APPDATA", "C:/Users/me/AppData/Roaming"),
            ]),
        );
        assert_eq!(
            windows.state,
            PathBuf::from("C:/Users/me/AppData/Local/domyjob/state")
        );
        assert_eq!(
            windows.config,
            PathBuf::from("C:/Users/me/AppData/Roaming/domyjob/config")
        );
        let unix = Dirs::of(
            Family::Unix,
            &set(&[("HOME", "/home/me"), ("LOCALAPPDATA", "/ignored")]),
        );
        assert_eq!(unix.state, PathBuf::from("/home/me/.local/state/domyjob"));
        let chosen = Dirs::of(Family::Unix, &set(&[("DOMYJOB_STATE", "/srv/dj")]));
        assert_eq!(chosen.state, PathBuf::from("/srv/dj"));
    }

    #[test]
    fn remote_invocations_suit_each_shell_family() {
        let key = crate::protocol::build_key();
        assert!(key.starts_with(crate::protocol::VERSION));
        assert_eq!(
            Family::Unix.invoke().as_arg_str(),
            format!("./.cache/domyjob/bin/domyjob-{key} node")
        );
        assert_eq!(
            Family::Windows.invoke().as_arg_str(),
            format!(".\\.cache\\domyjob\\bin\\domyjob-{key}.exe node")
        );
    }
}
