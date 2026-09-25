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
        let home = var("HOME")
            .or_else(|| var("USERPROFILE"))
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        let pick = |own: &str, xdg: &str, fallback: &[&str], windows: &str, windows_sub: &str| {
            if let Some(explicit) = var(own) {
                return PathBuf::from(explicit);
            }
            if cfg!(windows)
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
