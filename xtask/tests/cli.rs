use std::process::{Output, Stdio};

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "CLI integration tests start the built repository task executable"
    )]

    pub(super) fn command() -> std::process::Command {
        std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
    }
}

fn invoke(args: &[&str]) -> std::io::Result<Output> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()?;
    raw::command()
        .env("MISE_TRUSTED_CONFIG_PATHS", root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(std::process::Child::wait_with_output)
}

#[test]
fn cli_usage_and_workflow_checks_keep_their_exit_codes() {
    let usage = invoke(&["unknown"]).unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(usage.stdout.len(), 0);
    assert!(
        String::from_utf8(usage.stderr)
            .unwrap()
            .starts_with("usage: cargo xtask gates")
    );
    let workflows = invoke(&["workflows"]).unwrap();
    assert!(
        workflows.status.success(),
        "{}",
        String::from_utf8_lossy(&workflows.stderr)
    );
}
