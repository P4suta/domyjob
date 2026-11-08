use std::path::{Path, PathBuf};

use domyjob_core::client_cleanup::{self, JsonClient};

use crate::platform::user_files;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RetirementError {
    #[error(transparent)]
    File(#[from] user_files::UserFileError),
    #[error("{path}: invalid client configuration: {source}")]
    Cleanup {
        path: PathBuf,
        source: client_cleanup::CleanupError,
    },
    #[error("reading client configuration locations: {0}")]
    Io(#[from] std::io::Error),
    #[error("{path}: the Codex configuration is invalid")]
    Toml {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
}

fn directory(variable: &str, fallback: PathBuf) -> PathBuf {
    match crate::platform::variable(variable) {
        Some(path) => PathBuf::from(path),
        None => fallback,
    }
}

fn hooks(path: &Path, changed: &mut Vec<PathBuf>) -> Result<(), RetirementError> {
    if let Some(document) = user_files::read(path)?
        && let Some(update) =
            client_cleanup::retire_hooks(&document).map_err(|source| RetirementError::Cleanup {
                path: path.to_path_buf(),
                source,
            })?
    {
        write_update(path, update.as_str(), changed)?;
    }
    Ok(())
}

fn json_mcp(
    path: &Path,
    client: JsonClient,
    changed: &mut Vec<PathBuf>,
) -> Result<(), RetirementError> {
    if let Some(document) = user_files::read(path)?
        && let Some(update) = client_cleanup::retire_mcp(&document, client).map_err(|source| {
            RetirementError::Cleanup {
                path: path.to_path_buf(),
                source,
            }
        })?
    {
        write_update(path, update.as_str(), changed)?;
    }
    Ok(())
}

fn write_update(
    path: &Path,
    document: &str,
    changed: &mut Vec<PathBuf>,
) -> Result<(), RetirementError> {
    user_files::write(path, document.as_bytes())?;
    changed.push(path.to_path_buf());
    Ok(())
}

fn codex_mcp(document: &str) -> Result<Option<String>, toml_edit::TomlError> {
    let mut parsed: toml_edit::DocumentMut = document.parse()?;
    let Some(servers) = parsed
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return Ok(None);
    };
    let Some(server) = servers
        .get("domyjob")
        .and_then(toml_edit::Item::as_table_like)
    else {
        return Ok(None);
    };
    let program = server.get("command").and_then(toml_edit::Item::as_str);
    let arguments: Option<Vec<String>> = server
        .get("args")
        .and_then(toml_edit::Item::as_array)
        .and_then(|values| {
            values
                .iter()
                .map(|value| value.as_str().map(String::from))
                .collect()
        });
    let owned = program
        .zip(arguments)
        .is_some_and(|(program, arguments)| client_cleanup::owns_mcp(program, &arguments));
    if !owned {
        return Ok(None);
    }
    servers.remove("domyjob");
    Ok(Some(parsed.to_string()))
}

fn retire_at(
    home: &Path,
    codex: &Path,
    claude: &Path,
    opencode: &Path,
) -> Result<Vec<PathBuf>, RetirementError> {
    let mut changed = Vec::new();
    hooks(&codex.join("hooks.json"), &mut changed)?;
    hooks(&claude.join("settings.json"), &mut changed)?;
    json_mcp(&home.join(".claude.json"), JsonClient::Claude, &mut changed)?;
    for file in ["opencode.json", "opencode.jsonc"] {
        json_mcp(&opencode.join(file), JsonClient::OpenCode, &mut changed)?;
    }
    let config = codex.join("config.toml");
    if let Some(document) = user_files::read(&config)?
        && let Some(update) = codex_mcp(&document).map_err(|source| RetirementError::Toml {
            path: config.clone(),
            source,
        })?
    {
        write_update(&config, &update, &mut changed)?;
    }
    Ok(changed)
}

pub(crate) fn retire() -> Result<Vec<PathBuf>, RetirementError> {
    let home = crate::platform::home()?;
    let codex = directory("CODEX_HOME", home.join(".codex"));
    let claude = directory("CLAUDE_CONFIG_DIR", home.join(".claude"));
    let opencode = directory("XDG_CONFIG_HOME", home.join(".config")).join("opencode");
    let mut changed = retire_at(&home, &codex, &claude, &opencode)?;
    if let Some(path) = crate::platform::variable("OPENCODE_CONFIG") {
        json_mcp(Path::new(&path), JsonClient::OpenCode, &mut changed)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::{codex_mcp, retire_at};

    #[test]
    fn malformed_configuration_names_the_file_without_disclosing_its_content() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let codex = home.join("codex");
        let path = codex.join("config.toml");
        let private = "token = 'private-canary";
        crate::testing::write(&path, private);
        let error =
            retire_at(home, &codex, &home.join("claude"), &home.join("opencode")).unwrap_err();
        let detail = error.to_string();
        assert!(detail.contains("config.toml"));
        assert!(!detail.contains("private-canary"));
        assert_eq!(crate::testing::read(&path), private);
    }

    #[test]
    fn codex_cleanup_preserves_comments_other_servers_and_disabled_ownership() {
        let input = "# keep the owner choice\nmodel = 'model-name'\n\n[mcp_servers.other]\nurl = 'https://example.invalid'\n\n[mcp_servers.domyjob]\ncommand = '/bin/domyjob'\nargs = ['mcp']\nenabled = false\n";
        let output = codex_mcp(input).unwrap().unwrap();
        assert_eq!(
            output,
            "# keep the owner choice\nmodel = 'model-name'\n\n[mcp_servers.other]\nurl = 'https://example.invalid'\n"
        );
        assert!(codex_mcp(&output).unwrap().is_none());
        assert!(
            codex_mcp("[mcp_servers.domyjob]\ncommand = 'other'\nargs = ['mcp']\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn client_retirement_preserves_history_and_unrelated_configuration() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let codex = home.join("codex");
        let claude = home.join("claude");
        let opencode = home.join("opencode");
        let history = home.join("state/chat/chat.redb");
        crate::testing::write(&history, b"retained history");
        crate::testing::write(
            &codex.join("config.toml"),
            "model = 'keep'\n[mcp_servers.domyjob]\ncommand = '/bin/domyjob'\nargs = ['mcp']\n",
        );
        crate::testing::write(
            &home.join(".claude.json"),
            r#"{"mcpServers":{"domyjob":{"command":"other","args":["mcp"]}},"keep":true}"#,
        );
        let changed = retire_at(home, &codex, &claude, &opencode).unwrap();
        assert_eq!(changed, [codex.join("config.toml")]);
        assert_eq!(crate::testing::read(&history), "retained history");
        assert!(crate::testing::read(&home.join(".claude.json")).contains("other"));
        assert_eq!(
            retire_at(home, &codex, &claude, &opencode).unwrap(),
            Vec::<std::path::PathBuf>::new()
        );
    }
}
