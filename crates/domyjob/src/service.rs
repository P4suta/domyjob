use std::path::{Path, PathBuf};

use crate::paths::Dirs;
use crate::template::{Arg, Argv, Bindings, Text};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Files(crate::failure::IoFailure),
    #[error(transparent)]
    Definition(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Template(#[from] crate::template::TemplateError),
    #[error("the service definition has an empty command")]
    EmptyCommand,
    #[error("{program} could not start: {source}")]
    Start {
        program: String,
        source: std::io::Error,
    },
    #[error("{command} failed")]
    Failed { command: String },
    #[error(
        "starting with the session is not supported on {0}; run `domyjob serve` from your own service manager"
    )]
    Unsupported(&'static str),
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn definition_for(os: &'static str) -> Result<crate::config::ServiceConf, ServiceError> {
    crate::config::Config::builtin()?
        .services
        .remove(os)
        .ok_or(ServiceError::Unsupported(os))
}

fn bindings(dirs: &Dirs, exe: &Path, args: &[Arg]) -> Bindings {
    let action = Arg::concat(&[
        Arg::literal("\""),
        Arg::path(exe),
        Arg::literal("\" serve "),
        Arg::spaced(args),
    ]);
    let mut plist_arguments = String::new();
    for arg in args {
        plist_arguments.push_str("<string>");
        plist_arguments.push_str(&xml(arg.as_arg_str()));
        plist_arguments.push_str("</string>");
    }
    let log = dirs.state.join("serve.log");
    Bindings::new()
        .with("home", Arg::path(&dirs.home))
        .with("state", Arg::path(&dirs.state))
        .with("exe", Arg::path(exe))
        .with(
            "exe_xml",
            Arg::authorized_job_text(xml(&exe.display().to_string())),
        )
        .with("arguments", Arg::spaced(args))
        .with("plist_arguments", Arg::authorized_job_text(plist_arguments))
        .with("log", Arg::path(&log))
        .with(
            "log_xml",
            Arg::authorized_job_text(xml(&log.display().to_string())),
        )
        .with(
            "uid",
            Arg::number(u64::from(crate::platform::numeric_user_id())),
        )
        .with("action", action)
}

fn render_path(path: &Text, bindings: &Bindings) -> Result<PathBuf, ServiceError> {
    Ok(PathBuf::from(path.render(bindings)?.into_string()))
}

fn rendered(argv: &Argv, bindings: &Bindings) -> Result<crate::spawn::Invocation, ServiceError> {
    crate::spawn::Invocation::from_words(argv.render(bindings)?).ok_or(ServiceError::EmptyCommand)
}

fn status(
    argv: &Argv,
    bindings: &Bindings,
) -> Result<(bool, crate::spawn::Invocation), ServiceError> {
    let invocation = rendered(argv, bindings)?;
    let program = invocation.program().as_arg_str().to_owned();
    let result = invocation
        .command()
        .status()
        .map_err(|source| ServiceError::Start { program, source })?;
    Ok((result.success(), invocation))
}

fn run(action: &crate::config::ServiceAction, bindings: &Bindings) -> Result<(), ServiceError> {
    let (succeeded, invocation) = status(&action.run, bindings)?;
    if succeeded {
        return Ok(());
    }
    if action.tolerate_failure {
        if let Some(note) = &action.note {
            eprintln!("domyjob: {note}");
        }
        return Ok(());
    }
    Err(ServiceError::Failed {
        command: invocation.display(),
    })
}

fn execute(
    actions: &[crate::config::ServiceAction],
    bindings: &Bindings,
) -> Result<(), ServiceError> {
    for action in actions {
        run(action, bindings)?;
    }
    Ok(())
}

pub fn install(dirs: &Dirs, exe: &Path, args: &[Arg]) -> Result<String, ServiceError> {
    let definition = definition_for(crate::platform::OS)?;
    let bindings = bindings(dirs, exe, args);
    if let (Some(path), Some(contents)) = (&definition.file, &definition.contents) {
        let path = render_path(path, &bindings)?;
        let contents = contents.render(&bindings)?.into_string();
        crate::user_files::write(&path, contents.as_bytes()).map_err(ServiceError::Files)?;
    }
    execute(&definition.install, &bindings)?;
    Ok(definition.installed.render(&bindings)?.into_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uninstalled {
    Nothing,
    Service,
}

fn is_installed(
    definition: &crate::config::ServiceConf,
    bindings: &Bindings,
) -> Result<bool, ServiceError> {
    if let Some(path) = &definition.file {
        return crate::user_files::present(&render_path(path, bindings)?)
            .map_err(ServiceError::Files);
    }
    match &definition.probe {
        Some(probe) => status(probe, bindings).map(|(succeeded, _)| succeeded),
        None => Ok(false),
    }
}

pub fn uninstall(dirs: &Dirs) -> Result<Uninstalled, ServiceError> {
    let definition = definition_for(crate::platform::OS)?;
    let exe = crate::proc::own_executable().unwrap_or_else(|_| PathBuf::from("domyjob"));
    let bindings = bindings(dirs, &exe, &[]);
    if !is_installed(&definition, &bindings)? {
        return Ok(Uninstalled::Nothing);
    }
    execute(&definition.uninstall, &bindings)?;
    if let Some(path) = &definition.file {
        crate::user_files::remove(&render_path(path, &bindings)?).map_err(ServiceError::Files)?;
    }
    Ok(Uninstalled::Service)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> Dirs {
        Dirs {
            home: PathBuf::from("/home/me"),
            state: PathBuf::from("/home/me/.local/state/domyjob"),
            config: PathBuf::from("/home/me/.config/domyjob"),
            cache: PathBuf::from("/home/me/.cache/domyjob"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        }
    }

    #[test]
    fn service_definitions_name_the_binary_and_address() {
        let args = vec![Arg::literal("--expose"), Arg::literal("tailnet")];
        let bindings = bindings(&dirs(), Path::new("/opt/a&b/domyjob"), &args);
        let definitions = crate::config::Config::builtin().unwrap();

        let linux = definitions.services.get("linux").unwrap();
        let unit = linux.contents.as_ref().unwrap().render(&bindings).unwrap();
        assert!(
            unit.as_str()
                .contains("/opt/a&b/domyjob\" serve --expose tailnet")
        );

        let macos = definitions.services.get("macos").unwrap();
        let plist = macos.contents.as_ref().unwrap().render(&bindings).unwrap();
        assert!(
            plist
                .as_str()
                .contains("<string>--expose</string><string>tailnet</string>")
        );
        assert!(
            plist
                .as_str()
                .contains("<string>/opt/a&amp;b/domyjob</string>")
        );
        assert!(plist.as_str().contains("<key>KeepAlive</key><true/>"));

        assert!(definitions.services.get("windows").unwrap().file.is_none());
    }
}
