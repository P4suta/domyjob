use std::path::{Path, PathBuf};

use crate::world::{Machine, World, wait_for};
use crate::{Context as _, Failure};

const LONG_COMMAND_MILLIS: &str = "120000";

fn job_directory(machine: &Machine<'_>, job: &str) -> Result<PathBuf, Failure> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(machine.state()).context("reading the fixture state")? {
        let entry = entry.context("reading a fixture runner")?;
        if entry
            .file_type()
            .context("inspecting a fixture runner")?
            .is_dir()
            && entry.file_name().to_string_lossy().starts_with("runner-")
        {
            let path = entry.path().join("jobs").join(job);
            if crate::present(&path)? {
                found.push(path);
            }
        }
    }
    ensure!(
        found.len() == 1,
        "expected one owned job directory: {found:?}"
    );
    found
        .pop()
        .ok_or_else(|| Failure::new("the fixture job directory is missing"))
}

fn worker_for(world: &World, job: &str) -> Result<u32, Failure> {
    let mut workers = Vec::new();
    for (pid, command) in world.processes()? {
        let words: Vec<_> = command
            .split_whitespace()
            .map(|word| word.trim_matches('"'))
            .collect();
        if words.windows(2).any(|pair| pair == ["worker", job]) {
            workers.push(pid);
        }
    }
    ensure!(
        workers.len() == 1,
        "expected one supervisor belonging to this fixture: {workers:?}"
    );
    workers
        .pop()
        .ok_or_else(|| Failure::new("the fixture supervisor is missing"))
}

fn stopped_and_retired(world: &World, directory: &Path) -> Result<bool, Failure> {
    let quiet = crate::present(&directory.join("quiesced"))?;
    let retired = !crate::present(&directory.join("workspace"))?;
    let command_alive = world
        .processes()?
        .iter()
        .any(|(_, command)| command.contains("sleeper") && command.contains(LONG_COMMAND_MILLIS));
    ensure!(
        !quiet || !command_alive,
        "completion marker published while the owned command was alive"
    );
    ensure!(
        !retired || quiet,
        "workspace removed without a completion marker"
    );
    Ok(quiet && retired && !command_alive)
}

fn cache_targets(machine: &Machine<'_>) -> Result<Vec<PathBuf>, Failure> {
    let mut targets = Vec::new();
    for project in std::fs::read_dir(machine.state().join("build-cache"))
        .context("reading the fixture cache")?
    {
        let project = project.context("reading a cache project")?;
        if !project
            .file_type()
            .context("inspecting a cache project")?
            .is_dir()
        {
            continue;
        }
        let (keys, target) = if matches!(
            project.file_name().as_encoded_bytes().len(),
            domyjob_core::cache_key::ADDRESS_BYTES | domyjob_core::cache_key::ROUTE_BYTES
        ) {
            ("k", "t")
        } else {
            ("keys", "target")
        };
        for slot in
            std::fs::read_dir(project.path().join(keys)).context("reading the cache keys")?
        {
            let slot = slot.context("reading a cache key")?;
            targets.push(slot.path().join(target));
        }
    }
    Ok(targets)
}

pub(crate) fn supervisor_loss(world: &World) -> Result<(), Failure> {
    let client = world.machine("alpha");
    let node = world.machine("beta");
    crate::os::job_handoff(world, &client)?;
    let job = client
        .run(&["run", "beta", "--", "sleeper", LONG_COMMAND_MILLIS])?
        .exited(0)?
        .stdout
        .trim()
        .to_owned();
    crate::scenarios::job_phase(&client, &job, "running")?;
    let (_, identifier) = job
        .split_once(':')
        .ok_or_else(|| Failure::new("invalid fixture job reference"))?;
    let directory = job_directory(&node, identifier)?;
    let original_targets = cache_targets(&node)?;
    ensure!(
        original_targets.len() == 1,
        "one snapshot must have one cache target: {original_targets:?}"
    );
    let original_target = original_targets
        .first()
        .ok_or_else(|| Failure::new("the original cache target is missing"))?;
    ensure!(
        crate::present(&directory.join("workspace"))?,
        "running snapshot has no workspace"
    );
    let supervisor = worker_for(world, identifier)?;
    ensure!(
        crate::kill_process(supervisor)
            .context("stopping the fixture supervisor")?
            .success(),
        "fixture supervisor could not be stopped"
    );
    std::fs::write(client.work().join("Cargo.lock"), b"replacement cache input")
        .context("changing the cache key after supervisor loss")?;
    client
        .run(&["run", "beta", "--wait", "--keep", "--", "sleeper", "0"])?
        .exited(0)?;
    ensure!(
        crate::present(original_target)?,
        "worker loss retired a cache use before native quiescence reconciliation"
    );
    ensure!(
        cache_targets(&node)?.len() == 2,
        "replacement did not preserve the unconfirmed original cache use"
    );
    crate::scenarios::job_phase(&client, &job, "finished lost")?;
    wait_for("the owned job to stop before workspace retirement", || {
        client.run(&["status", &job, "--keep"])?.exited(0)?;
        Ok(stopped_and_retired(world, &directory)?.then_some(()))
    })?;
    ensure!(
        !crate::present(original_target)?,
        "confirmed supervisor loss retained its superseded cache target"
    );
    ensure!(
        cache_targets(&node)?.len() == 1,
        "only the latest idle cache should remain after reconciliation"
    );
    let state = client.run(&["wait", &job, "--keep"])?;
    ensure!(
        state.code == Some(1) && state.stdout.contains("finished lost"),
        "supervisor loss reported an incorrect outcome: {state}"
    );
    client.run(&["logs", &job])?.exited(0)?;
    let missing = client.run(&["logs", &job])?;
    ensure!(
        missing.code == Some(1),
        "consumed output was fabricated: {missing}"
    );
    Ok(())
}
