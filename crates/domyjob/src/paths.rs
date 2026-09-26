use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub home: PathBuf,
    pub state: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    pub keys: crate::keystore::KeyStore,
}

fn var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

impl Dirs {
    #[must_use]
    pub fn from_env() -> Self {
        Self::of(crate::platform::FAMILY, &var)
    }

    #[must_use]
    pub fn of(family: Family, var: &dyn Fn(&str) -> Option<OsString>) -> Self {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Unix,
    Windows,
}

impl Family {
    #[must_use]
    pub const fn links(self) -> bool {
        matches!(self, Self::Unix)
    }

    #[must_use]
    pub const fn modes(self) -> bool {
        matches!(self, Self::Unix)
    }

    #[must_use]
    pub const fn load_average(self) -> bool {
        matches!(self, Self::Unix)
    }

    #[must_use]
    pub const fn agent_socket(self) -> bool {
        matches!(self, Self::Unix)
    }

    #[must_use]
    pub const fn replaces_running_executables(self) -> bool {
        matches!(self, Self::Unix)
    }

    #[must_use]
    pub fn remote_bin(self) -> crate::template::Arg {
        use crate::protocol::VERSION;
        match self {
            Self::Unix => {
                crate::template::Arg::joined(&[".cache/domyjob/bin/", VERSION, "/domyjob"])
            }
            Self::Windows => {
                crate::template::Arg::joined(&[".cache/domyjob/bin/", VERSION, "/domyjob.exe"])
            }
        }
    }

    #[must_use]
    pub fn invoke(self) -> crate::template::Arg {
        use crate::protocol::VERSION;
        match self {
            Self::Unix => {
                crate::template::Arg::joined(&["./.cache/domyjob/bin/", VERSION, "/domyjob node"])
            }
            Self::Windows => crate::template::Arg::joined(&[
                ".\\.cache\\domyjob\\bin\\",
                VERSION,
                "\\domyjob.exe node",
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
        let version = crate::protocol::VERSION;
        assert_eq!(
            Family::Unix.invoke().as_arg_str(),
            format!("./.cache/domyjob/bin/{version}/domyjob node")
        );
        assert_eq!(
            Family::Windows.invoke().as_arg_str(),
            format!(".\\.cache\\domyjob\\bin\\{version}\\domyjob.exe node")
        );
    }
}
