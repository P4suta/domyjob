use serde_json::{Value, json};

use crate::world::{Machine, World, wait_for};
use crate::{Failure, Scenario};

pub(crate) const ALL: [Scenario; 9] = [
    Scenario {
        name: "retired_chat_commands_are_rejected_without_side_effects",
        machines: &["alpha"],
        run: retired_chat_commands_are_rejected_without_side_effects,
    },
    Scenario {
        name: "failure_diagnostics_keep_only_bounded_owned_service_logs",
        machines: &["alpha"],
        run: failure_diagnostics_keep_only_bounded_owned_service_logs,
    },
    Scenario {
        name: "failure_diagnostics_sample_only_the_owned_service_before_cleanup",
        machines: &["alpha"],
        run: crate::world::service_diagnostic_fixture,
    },
    Scenario {
        name: "bootstrap_preserves_first_request_and_cleans_its_owned_work",
        machines: &["alpha", "beta"],
        run: crate::bootstrap::scenario,
    },
    Scenario {
        name: "fake_ssh_stands_in_for_openssh",
        machines: &["alpha", "beta"],
        run: fake_ssh_stands_in_for_openssh,
    },
    Scenario {
        name: "kept_job_reads_preserve_success_and_failure_results",
        machines: &["alpha", "beta"],
        run: kept_job_reads_preserve_success_and_failure_results,
    },
    Scenario {
        name: "jobs_run_through_the_node_wrapper",
        machines: &["alpha", "beta"],
        run: jobs_run_through_the_node_wrapper,
    },
    Scenario {
        name: "a_lost_supervisor_reaps_its_owned_job_before_retirement",
        machines: &["alpha", "beta"],
        run: crate::quiescence::supervisor_loss,
    },
    Scenario {
        name: "resource_probe_recognizes_its_parent_scope",
        machines: &[],
        run: resource_probe_recognizes_its_parent_scope,
    },
];

fn retired_chat_commands_are_rejected_without_side_effects(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    for arguments in [
        vec![],
        vec!["chat", "directory"],
        vec!["mcp"],
        vec!["tui"],
        vec!["chat-worker", "--agent", "helper"],
    ] {
        alpha.run(&arguments)?.exited(2)?;
    }
    let help = alpha.run(&["--help"])?;
    ensure!(
        help.exited(0)?.stdout.contains("service") && !help.stdout.contains("\n  chat "),
        "help must expose the job service: {help}"
    );
    ensure!(
        !alpha.state().exists(),
        "rejected commands and help must not initialize state"
    );
    ensure!(
        world.ssh_log()?.is_empty(),
        "rejected commands and help must not contact a peer"
    );
    Ok(())
}

fn failure_diagnostics_keep_only_bounded_owned_service_logs(world: &World) -> Result<(), Failure> {
    const OVERSIZED_LOG_BYTES: usize = 8192;
    const EXTRA_LOGS: usize = 10;
    const MAX_CAPTURED_LOGS: usize = 8;
    let service = world.root().join("alpha-service-serve.log");
    std::fs::write(&service, vec![b'x'; OVERSIZED_LOG_BYTES])
        .map_err(|error| Failure::new(format!("writing the oversized service log: {error}")))?;
    let job = world
        .machine("alpha")
        .state()
        .join("runner-fixture/jobs/fixture");
    std::fs::create_dir_all(job.join("workspace"))
        .map_err(|error| Failure::new(format!("preparing diagnostic fixtures: {error}")))?;
    for (path, bytes) in [
        (
            job.join("supervisor.log"),
            b"worker startup cause".as_slice(),
        ),
        (job.join("output.log"), b"private job output".as_slice()),
        (
            job.join("workspace/supervisor.log"),
            b"untrusted workspace log".as_slice(),
        ),
    ] {
        std::fs::write(path, bytes)
            .map_err(|error| Failure::new(format!("writing diagnostic fixtures: {error}")))?;
    }
    let diagnostics = world.failure_diagnostics()?;
    ensure!(
        diagnostics.contains("alpha-service-serve.log")
            && diagnostics.contains("omitted]")
            && diagnostics.contains("worker startup cause")
            && !diagnostics.contains("private job output")
            && !diagnostics.contains("untrusted workspace log")
            && !diagnostics
                .contains(&"x".repeat(crate::world::FAILURE_LOG_BYTES.saturating_add(1))),
        "failure diagnostics must retain bounded service causes without job payloads"
    );
    for index in 0..EXTRA_LOGS {
        std::fs::write(
            world.root().join(format!("{index}-service-serve.log")),
            b"bounded service marker",
        )
        .map_err(|error| Failure::new(format!("writing service count fixtures: {error}")))?;
    }
    let limited = world.failure_diagnostics()?;
    ensure!(
        limited.matches("--- ").count() == MAX_CAPTURED_LOGS
            && limited.contains("[diagnostic scan limit reached]"),
        "diagnostics must cap the number of captured service logs"
    );
    let binaries = crate::world::prepare_binaries()?;
    let captured = match crate::attempt(
        &Scenario {
            name: "controlled_failure_diagnostic",
            machines: &["alpha"],
            run: controlled_failure_diagnostic,
        },
        binaries.path(),
        false,
    ) {
        Err(error) => error,
        Ok(()) => {
            return Err(Failure::new(
                "the controlled diagnostic failure disappeared",
            ));
        }
    };
    ensure!(
        captured
            .0
            .starts_with("original controlled scenario failure")
            && captured.0.contains("owned failure diagnostics:")
            && captured.0.contains("controlled service cause"),
        "the real failure pipeline must retain its original cause and capture logs before cleanup"
    );
    Ok(())
}

fn controlled_failure_diagnostic(world: &World) -> Result<(), Failure> {
    std::fs::write(
        world.root().join("alpha-service-serve.log"),
        b"controlled service cause",
    )
    .map_err(|error| Failure::new(format!("writing the controlled failure log: {error}")))?;
    Err(Failure::new("original controlled scenario failure"))
}

fn fake_ssh_stands_in_for_openssh(world: &World) -> Result<(), Failure> {
    crate::ssh::check_recognizer()?;
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    let ready = alpha.run(&["doctor", "beta"])?;
    ensure!(
        ready.code == Some(0) && ready.stdout.starts_with("beta: ready"),
        "a reachable machine should be ready: {ready}"
    );
    beta.set_offline(true)?;
    let offline = alpha.run(&["doctor", "beta"])?;
    ensure!(
        offline.code == Some(1) && offline.stderr.contains("Connection refused"),
        "an offline machine should refuse the connection: {offline}"
    );
    beta.set_offline(false)?;
    beta.set_drop_reply(true)?;
    let dropped = alpha.run(&["doctor", "beta"])?;
    ensure!(
        dropped.code == Some(1) && dropped.stderr.contains("closed by remote host"),
        "a lost reply should fail the call: {dropped}"
    );
    beta.set_drop_reply(false)?;
    let unknown = alpha.run(&["doctor", "gamma"])?;
    ensure!(
        unknown.code == Some(1) && unknown.stderr.contains("Could not resolve hostname gamma"),
        "an unknown machine should not resolve: {unknown}"
    );
    let log = world.ssh_log()?;
    let outcomes = log
        .iter()
        .map(|entry| text(entry, "/outcome"))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        outcomes == ["probe", "node", "offline", "dropped", "unknown-host"],
        "the shell should be probed once, then every call should reach the node: {log:#?}"
    );
    for entry in &log {
        ensure!(
            text(entry, "/from")? == "alpha"
                && strings(entry, "/options")?.contains(&"BatchMode=yes"),
            "domyjob should connect from alpha in batch mode: {entry}"
        );
    }
    Ok(())
}

fn kept_job_reads_preserve_success_and_failure_results(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    crate::os::job_handoff(world, &alpha)?;
    for (code, argument) in [(0, "0"), (7, "7")] {
        let submitted = alpha.run(&["on", "beta", "--", "sleeper", "0", argument])?;
        let job = submitted.exited(0)?.stdout.trim();
        let waited = alpha.run(&["wait", "--keep", job])?;
        waited.exited(code)?;
        let before = alpha.run(&["ls", "beta"])?;
        for arguments in [vec!["status", "--keep", job], vec!["status", job, "--keep"]] {
            let status = alpha.run(&arguments)?;
            ensure!(
                status.exited(0)?.stdout == waited.stdout,
                "unstable retained status: {status}"
            );
        }
        for _ in 0..2 {
            let logs = alpha.run(&["logs", "--keep", job])?;
            ensure!(
                logs.exited(0)?.stdout == "sleeper slept 0ms\n",
                "missing retained logs: {logs}"
            );
            let repeated = alpha.run(&["wait", job, "--keep"])?;
            ensure!(
                repeated.exited(code)?.stdout == waited.stdout,
                "unstable retained result: {repeated}"
            );
        }
        let after = alpha.run(&["ls", "beta"])?;
        ensure!(
            before.exited(0)?.stdout == after.exited(0)?.stdout,
            "kept reads changed inventory: {after}"
        );
        let id = job
            .rsplit_once(':')
            .ok_or_else(|| Failure::new("invalid job reference"))?
            .1;
        let cleaned = alpha.run(&["clean", "beta", "--job", id])?;
        ensure!(
            cleaned.exited(0)?.stdout.contains("cleaned 1"),
            "explicit cleanup must retire kept results: {cleaned}"
        );
        let receipt = alpha.run(&["status", job, "--keep"])?;
        ensure!(
            receipt.exited(0)?.stdout == waited.stdout,
            "cleanup must preserve the completion receipt: {receipt}"
        );
        alpha.run(&["logs", job, "--keep"])?.exited(1)?;
    }
    Ok(())
}

fn jobs_run_through_the_node_wrapper(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    crate::os::job_handoff(world, &alpha)?;
    let on = alpha.run(&["on", "beta", "--wait", "--", "sleeper", "0", "7"])?;
    ensure!(
        on.code == Some(7) && on.stdout.contains("sleeper slept 0ms"),
        "`on` should return the job's exit status and output: {on}"
    );
    let run = alpha.run(&["run", "beta", "--wait", "--", "sleeper", "10"])?;
    ensure!(
        run.code == Some(0) && run.stdout.contains("sleeper slept 10ms"),
        "`run` should run the job in a snapshot of this directory: {run}"
    );
    let nodes = world
        .ssh_log()?
        .iter()
        .filter(|entry| entry.get("outcome") == Some(&json!("node")))
        .count();
    ensure!(
        nodes >= 4,
        "the jobs should have gone through the node wrapper"
    );
    consumed_submissions_do_not_execute_again(&alpha, &world.machine("beta"))?;
    linux_host_admission(world, &alpha)
}

fn consumed_submissions_do_not_execute_again(
    client: &Machine<'_>,
    server: &Machine<'_>,
) -> Result<(), Failure> {
    const SUBMISSION: &str = "dddddddddddddddddddddddddddddddd";
    let first = client.run(&[
        "on",
        "beta",
        "--submission",
        SUBMISSION,
        "--wait",
        "--keep",
        "--",
        "sleeper",
        "0",
    ])?;
    ensure!(
        first.exited(0)?.stdout.contains("sleeper slept 0ms"),
        "the initial job must execute: {first}"
    );
    let reference = first
        .stdout
        .lines()
        .next()
        .ok_or_else(|| Failure::new("missing job reference"))?;
    client.run(&["logs", reference])?.exited(0)?;
    let gone = client.run(&["logs", reference])?;
    ensure!(
        gone.code == Some(1) && gone.stderr.contains("MissingJob"),
        "consumed logs must be explicitly unavailable: {gone}"
    );
    let repeated = client.run(&[
        "on",
        "beta",
        "--submission",
        SUBMISSION,
        "--wait",
        "--",
        "sleeper",
        "0",
    ])?;
    ensure!(
        repeated.exited(0)?.stdout.contains("finished succeeded")
            && !repeated.stdout.contains("sleeper slept"),
        "a consumed submission must return its receipt without executing again: {repeated}"
    );
    let kept = client.run(&["on", "beta", "--wait", "--keep", "--", "sleeper", "0"])?;
    let retained = kept
        .exited(0)?
        .stdout
        .lines()
        .next()
        .ok_or_else(|| Failure::new("missing retained job reference"))?;
    unacknowledged_retrievals_preserve_logs(client, server, retained)?;
    client.run(&["logs", retained])?.exited(0)?;
    Ok(())
}

fn unacknowledged_retrievals_preserve_logs(
    client: &Machine<'_>,
    server: &Machine<'_>,
    retained: &str,
) -> Result<(), Failure> {
    server.set_drop_reply(true)?;
    let disconnected = client.run(&["wait", retained]);
    server.set_drop_reply(false)?;
    ensure!(
        disconnected?.code == Some(1),
        "a discarded reply must fail retrieval"
    );
    let unwritten = client.run_without_output_reader(&["wait", retained])?;
    ensure!(
        unwritten.code == Some(1)
            && unwritten
                .stderr
                .starts_with("domyjob: SSH or node I/O failed:"),
        "failed terminal output must fail retrieval: {unwritten}"
    );
    ensure!(
        client
            .run(&["logs", retained, "--keep"])?
            .exited(0)?
            .stdout
            .contains("sleeper slept"),
        "--keep must preserve logs after reading"
    );
    Ok(())
}

fn resource_parent_scope(cgroups: &str) -> Option<domyjob_core::domain::JobId> {
    cgroups.lines().find_map(|line| {
        let (_, scope) = line.strip_prefix("0::")?.rsplit_once("/domyjob.slice/")?;
        let id = scope.strip_prefix("domyjob-job-")?.strip_suffix(".scope")?;
        domyjob_core::domain::JobId::try_from(id.to_owned()).ok()
    })
}

fn resource_probe_recognizes_its_parent_scope(_world: &World) -> Result<(), Failure> {
    let scope = "domyjob-job-00000000000000000000000000000001.scope";
    for (membership, expected) in [
        (format!("0::/user.slice/domyjob.slice/{scope}\n"), true),
        (
            format!("1:memory:/user.slice/domyjob.slice/{scope}\n"),
            false,
        ),
        (format!("0::/user.slice/other.slice/{scope}\n"), false),
        (
            format!("0::/user.slice/domyjob.slice/{scope}/child\n"),
            false,
        ),
        (
            "0::/user.slice/domyjob.slice/domyjob-job-invalid.scope\n".to_owned(),
            false,
        ),
        (
            "0::/user.slice/app.slice/verification.scope\n".to_owned(),
            false,
        ),
    ] {
        ensure!(
            resource_parent_scope(&membership).is_some() == expected,
            "incorrect inherited admission for {membership:?}"
        );
    }
    Ok(())
}

const RESOURCE_PROBE_MILLISECONDS: u32 = 60_000;

fn resource_probe_capacity(limit: u32) -> Result<u32, Failure> {
    let membership = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| Failure::new(format!("reading the verification job's cgroup: {error}")))?;
    let inherited = u32::from(resource_parent_scope(&membership).is_some());
    let probes = limit
        .checked_sub(inherited)
        .ok_or_else(|| Failure::new("the verification job exceeds the host admission budget"))?;
    ensure!(
        probes > 0,
        "the verification job leaves no admission capacity"
    );
    Ok(probes)
}

fn linux_host_admission(world: &World, alpha: &Machine<'_>) -> Result<(), Failure> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let bytes = match std::fs::read("/etc/domyjob/resource-policy.json") {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Failure::new(format!("reading the host policy: {error}"))),
    };
    let policy: domyjob_core::resource_policy::Policy = domyjob_core::ingress::json(&bytes, 16_384)
        .map_err(|error| Failure::new(format!("validating the host policy: {error}")))?;
    let probes = resource_probe_capacity(policy.budget().concurrent)?;
    let beta = world.machine("beta");
    let sleeper = world.root().join("bin/sleeper");
    let program = sleeper
        .to_str()
        .ok_or_else(|| Failure::new("non-UTF8 sleeper path"))?;
    let mut jobs = Vec::new();
    let result = (|| {
        for slot in 0..probes {
            let (client, target) = if slot == 0 {
                (alpha, "beta")
            } else {
                (&beta, "alpha")
            };
            let milliseconds = RESOURCE_PROBE_MILLISECONDS.saturating_add(slot).to_string();
            let job = resource_probe(client, target, (program, &milliseconds), &mut jobs)?;
            job_phase(alpha, &job, "running")?;
        }
        for job in &jobs {
            let status = alpha.run(&["status", job, "--keep"])?;
            ensure!(
                status.exited(0)?.stdout.contains("running"),
                "admitted probes must run together: {status}"
            );
        }
        let queued = resource_probe(alpha, "beta", (program, "0"), &mut jobs)?;
        job_phase(alpha, &queued, "queued")?;
        let killed = alpha.run(&["kill", &queued])?;
        ensure!(
            killed.exited(0)?.stdout.contains("finished killed"),
            "queued cancellation failed: {killed}"
        );
        ensure!(
            alpha.run(&["logs", &queued])?.exited(0)?.stdout.is_empty(),
            "a cancelled queued command must never run"
        );

        let replacement = resource_probe(alpha, "beta", (program, "0"), &mut jobs)?;
        job_phase(alpha, &replacement, "queued")?;
        let interrupted = jobs
            .first()
            .ok_or_else(|| Failure::new("no admitted job"))?;
        interrupt_resource_probe(world, program, interrupted)?;
        ensure!(
            alpha
                .run(&["wait", &replacement, "--keep"])?
                .exited(0)?
                .stdout
                .contains("finished succeeded"),
            "a queued replacement must acquire the released slot"
        );
        job_phase(alpha, interrupted, "finished lost")
    })();
    let mut failures = Vec::new();
    for job in jobs {
        match alpha.run(&["kill", &job]) {
            Ok(cleanup) if cleanup.stdout.contains("finished") => {}
            Ok(cleanup) => failures.push(format!("job {job} did not finish: {cleanup}")),
            Err(error) => failures.push(format!("cleaning up job {job}: {error}")),
        }
    }
    ensure!(
        failures.is_empty(),
        "resource cleanup failed: {failures:?}; original result: {result:?}"
    );
    result
}

fn resource_probe(
    client: &Machine<'_>,
    target: &str,
    (program, milliseconds): (&str, &str),
    jobs: &mut Vec<String>,
) -> Result<String, Failure> {
    let job = client
        .run(&["on", target, "--", program, milliseconds])?
        .exited(0)?
        .stdout
        .trim()
        .to_owned();
    jobs.push(job.clone());
    Ok(job)
}

fn interrupt_resource_probe(world: &World, program: &str, job: &str) -> Result<(), Failure> {
    let family = wait_for("the admitted command to start", || {
        let found: Vec<_> = world
            .processes()?
            .into_iter()
            .filter(|(_, command)| {
                command.contains(program)
                    && command.contains(&format!(" {RESOURCE_PROBE_MILLISECONDS}"))
            })
            .collect();
        let started = found
            .iter()
            .any(|(_, command)| command.starts_with(program));
        Ok(started.then_some(found.into_iter().map(|(pid, _)| pid).collect::<Vec<_>>()))
    })?;
    let reference = domyjob_core::domain::JobReference::try_from(job.to_owned())
        .map_err(|error| Failure::new(format!("invalid job reference: {error}")))?;
    let worker = wait_for("the admitted job worker", || {
        Ok(world
            .processes()?
            .into_iter()
            .find(|(_, command)| command.contains(&format!(" worker {}", reference.job().as_str())))
            .map(|(pid, _)| pid))
    })?;
    ensure!(
        crate::kill_process(worker)
            .map_err(|error| Failure::new(format!("killing the job worker: {error}")))?
            .success(),
        "the worker could not be interrupted"
    );
    wait_for("the orphaned scope family to exit", || {
        Ok(world
            .processes()?
            .iter()
            .all(|(pid, _)| !family.contains(pid))
            .then_some(()))
    })
}

pub(crate) fn job_phase(client: &Machine<'_>, job: &str, phase: &str) -> Result<(), Failure> {
    wait_for(&format!("job {job} to become {phase}"), || {
        let status = client.run(&["status", job, "--keep"])?;
        Ok(status.exited(0)?.stdout.contains(phase).then_some(()))
    })
}

fn text<'value>(value: &'value Value, pointer: &str) -> Result<&'value str, Failure> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::new(format!("no text at {pointer} in {value:#}")))
}

fn strings<'value>(value: &'value Value, pointer: &str) -> Result<Vec<&'value str>, Failure> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .and_then(|items| items.iter().map(Value::as_str).collect())
        .ok_or_else(|| Failure::new(format!("no list of text at {pointer} in {value:#}")))
}
