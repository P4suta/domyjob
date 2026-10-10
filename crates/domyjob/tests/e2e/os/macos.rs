use std::path::Path;
use std::process::ExitCode;

use crate::world::{Machine, World, wait_for};
use crate::{Context as _, Failure};

pub(super) fn launchctl() -> ExitCode {
    let unavailable =
        std::env::var_os("HOME").is_some_and(|home| Path::new(&home).join("no-login").exists());
    if unavailable {
        return ExitCode::FAILURE;
    }
    if std::env::args().nth(1).as_deref() == Some("managername") {
        println!("Aqua");
    }
    ExitCode::SUCCESS
}

pub(super) fn job_handoff(world: &World, client: &Machine<'_>) -> Result<(), Failure> {
    let beta = world.machine("beta");
    let missing_agent = client.run(&["on", "beta", "--", "sleeper", "0"])?;
    ensure!(
        missing_agent.code == Some(1) && missing_agent.stderr.contains("login-session agent"),
        "a missing agent must refuse submission with its cause: {missing_agent}"
    );
    beta.start_background(&["chat", "serve"])?;
    wait_for("the login-session handoff listener", || {
        Ok(crate::present(&beta.state().join("chat/serve.pid"))?.then_some(()))
    })?;
    let marker = beta.home().join("no-login");
    std::fs::write(&marker, b"").context("simulating a missing GUI session")?;
    let missing_session = client.run(&["on", "beta", "--", "sleeper", "0"])?;
    std::fs::remove_file(&marker).context("restoring the GUI session")?;
    ensure!(
        missing_session.code == Some(1) && missing_session.stderr.contains("not logged in"),
        "a missing GUI session must refuse even while the agent exists: {missing_session}"
    );
    let job = client
        .run(&[
            "on",
            "beta",
            "--",
            "/bin/sh",
            "-c",
            "printf 'handoff-live\\n'; /bin/sleep 300 & wait",
        ])?
        .exited(0)?
        .stdout
        .trim()
        .to_owned();
    crate::scenarios::job_phase(client, &job, "running")?;
    wait_for("output from the handed-off command", || {
        Ok(client
            .run(&["logs", &job])?
            .exited(0)?
            .stdout
            .contains("handoff-live")
            .then_some(()))
    })?;
    let killed = client.run(&["kill", &job])?;
    ensure!(
        killed.exited(0)?.stdout.contains("finished killed"),
        "the handed-off process tree must be killed: {killed}"
    );
    crate::scenarios::job_phase(client, &job, "finished killed")?;
    ensure!(
        client
            .run(&["logs", &job])?
            .exited(0)?
            .stdout
            .contains("handoff-live"),
        "completed handoff logs must remain readable"
    );
    Ok(())
}
