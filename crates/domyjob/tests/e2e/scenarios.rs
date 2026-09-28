//! Scenarios against the chat of AI agents and the job runner.
//!
//! Every write names its agent with `--as`, and `chat setup` always skips the service and the AI client registrations, so no scenario touches the host's service manager or client configuration.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::provider::CODEX_SESSION;
use crate::world::{
    Machine, Run, World, calls, files_containing, same_directory, wait_for, wait_within,
};
use crate::{Failure, Scenario};

/// Every scenario, in the order the runner starts them.
pub(crate) const ALL: [Scenario; 12] = [
    Scenario {
        name: "fake_ssh_stands_in_for_openssh",
        machines: &["alpha", "beta"],
        run: fake_ssh_stands_in_for_openssh,
    },
    Scenario {
        name: "one_machine_answers_resumes_and_ranks",
        machines: &["alpha"],
        run: one_machine_answers_resumes_and_ranks,
    },
    Scenario {
        name: "failures_never_become_answers",
        machines: &["alpha"],
        run: failures_never_become_answers,
    },
    Scenario {
        name: "a_killed_worker_ends_the_asks_wait",
        machines: &["alpha"],
        run: a_killed_worker_ends_the_asks_wait,
    },
    Scenario {
        name: "asks_take_turns_per_agent_and_run_together_across_agents",
        machines: &["alpha", "beta"],
        run: asks_take_turns_per_agent_and_run_together_across_agents,
    },
    Scenario {
        name: "mcp_serves_an_interactive_agent",
        machines: &["alpha"],
        run: mcp_serves_an_interactive_agent,
    },
    Scenario {
        name: "three_machines_exchange_privately_and_converge",
        machines: &["alpha", "beta", "gamma"],
        run: three_machines_exchange_privately_and_converge,
    },
    Scenario {
        name: "racing_answer_and_withdrawal_agree_everywhere",
        machines: &["alpha", "beta"],
        run: racing_answer_and_withdrawal_agree_everywhere,
    },
    Scenario {
        name: "nested_asks_carry_the_chain_and_refuse_cycles",
        machines: &["alpha"],
        run: nested_asks_carry_the_chain_and_refuse_cycles,
    },
    Scenario {
        name: "cleaned_and_reset_machines_keep_syncing",
        machines: &["alpha", "beta", "gamma"],
        run: cleaned_and_reset_machines_keep_syncing,
    },
    Scenario {
        name: "background_service_delivers_without_commands",
        machines: &["alpha", "beta"],
        run: background_service_delivers_without_commands,
    },
    Scenario {
        name: "jobs_run_through_the_node_wrapper",
        machines: &["alpha", "beta"],
        run: jobs_run_through_the_node_wrapper,
    },
];

/// The fake SSH reaches a machine's node, and its markers and unknown names fail the way OpenSSH does.
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

/// A managed agent answers, resumes its session on the next turn, runs once per ask, and ranks in the directory.
fn one_machine_answers_resumes_and_ranks(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    join(&alpha, "lead", "claude")?;
    let (reviewer, directory) = start(
        &alpha,
        "reviewer",
        "codex",
        &[
            "--role",
            "code reviewer",
            "--skill",
            "rust",
            "--skill",
            "security",
        ],
    )?;
    start(
        &alpha,
        "builder",
        "claude",
        &["--role", "rust release builder"],
    )?;
    start(
        &alpha,
        "writer",
        "opencode",
        &["--description", "writes guides about rust and cargo"],
    )?;
    let (first, first_answer) = answered(&ask(&alpha, "lead", "reviewer", "hello")?)?;
    let (second, second_answer) = answered(&ask(&alpha, "lead", "reviewer", "hello again")?)?;
    ensure!(
        first_answer == "fake codex answer 1" && second_answer == "fake codex answer 2",
        "each ask should get its own call's answer: {first_answer:?}, {second_answer:?}"
    );
    world.settle()?;
    let made = calls(&directory)?;
    let turns = made
        .iter()
        .map(|call| text(call, "/turn"))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        turns == [first.as_str(), second.as_str()],
        "exactly one AI CLI call should run each ask: {made:#?}"
    );
    let arguments = made
        .iter()
        .map(|call| strings(call, "/args"))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        matches!(arguments.as_slice(), [fresh, resumed]
            if !fresh.contains(&"resume") && resumed.ends_with(&["resume", CODEX_SESSION, "-"])),
        "the second call should resume the session the first one reported: {made:#?}"
    );
    for call in &made {
        ensure!(
            call.get("overlap").is_none()
                && text(call, "/agent")? == reviewer
                && same_directory(Path::new(text(call, "/cwd")?), &directory)?,
            "calls should run one at a time for {reviewer} in its directory: {made:#?}"
        );
    }
    let ranked = alpha.chat(&["directory", "rust"])?;
    let listed = ranked.exited(0)?.json()?;
    let agents = labels(&listed, "/agents", "/agent")?;
    ensure!(
        agents == ["reviewer@local", "builder@local", "writer@local"],
        "a skill should rank before a role, and a role before a description: {ranked}"
    );
    Ok(())
}

/// Incomplete, failing, oversized, hijacked, missing, and interrupted turns never become answers, and no turn runs twice.
fn failures_never_become_answers(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    join(&alpha, "lead", "claude")?;
    let mut directories = Vec::new();
    for tool in ["claude", "codex", "opencode"] {
        let name = format!("{tool}-agent");
        directories.push(start(&alpha, &name, tool, &[])?.1);
        ended(&ask(&alpha, "lead", &name, "INCOMPLETE")?, "failed")?;
    }
    answered(&ask(&alpha, "lead", "codex-agent", "hello")?)?;
    for keyword in ["NONZERO", "HUGE", "CHANGE_SESSION"] {
        ended(&ask(&alpha, "lead", "codex-agent", keyword)?, "failed")?;
    }
    interrupted_without_rerun(world, &alpha)?;
    world.remove_fake("opencode")?;
    ended(&ask(&alpha, "lead", "opencode-agent", "hello")?, "failed")?;
    let log = alpha.worker_log();
    wait_for("the worker log to explain the missing AI CLI", || {
        Ok(
            files_containing(&alpha.state(), "opencode is not installed or not on PATH")?
                .contains(&log)
                .then_some(()),
        )
    })?;
    world.settle()?;
    let mut turns = Vec::new();
    for directory in &directories {
        for call in calls(directory)? {
            turns.push(text(&call, "/turn")?.to_owned());
        }
    }
    let count = turns.len();
    turns.sort();
    turns.dedup();
    ensure!(
        turns.len() == count,
        "a turn ran its AI CLI more than once: {turns:?}"
    );
    Ok(())
}

/// A worker killed during its turn takes the AI CLI's processes with it, and the next dispatch records the turn as interrupted without rerunning it.
fn interrupted_without_rerun(world: &World, alpha: &Machine<'_>) -> Result<(), Failure> {
    let request = pending(&ask_with(
        alpha,
        "lead",
        ("codex-agent", "KILL_PARENT"),
        "0",
    )?)?;
    let directory = alpha.work().join("codex-agent");
    let call = wait_for("the AI CLI to kill its worker", || {
        Ok(calls(&directory)?
            .into_iter()
            .find(|call| call.get("turn") == Some(&json!(request))))
    })?;
    let family = ["/pid", "/child"]
        .iter()
        .map(|pointer| {
            call.pointer(pointer)
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .ok_or_else(|| Failure::new(format!("no PID at {pointer} in {call}")))
        })
        .collect::<Result<Vec<u32>, _>>()?;
    let mut left = Vec::new();
    wait_for("the interrupted AI CLI and its child to end", || {
        left = world
            .processes()?
            .into_iter()
            .filter(|(pid, _)| family.contains(pid))
            .collect();
        Ok(left.is_empty().then_some(()))
    })
    .map_err(|failure| Failure::new(format!("{failure}; still running: {left:#?}")))?;
    answered(&ask(
        alpha,
        "lead",
        "codex-agent",
        "after the interruption",
    )?)?;
    ended(&alpha.chat(&["wait", &request])?, "interrupted")?;
    let runs = calls(&directory)?
        .iter()
        .filter(|recorded| recorded.get("turn") == Some(&json!(request)))
        .count();
    ensure!(
        runs == 1,
        "the interrupted turn ran its AI CLI {runs} times"
    );
    Ok(())
}

/// The asker waiting on a turn whose worker was killed learns that the turn was interrupted.
fn a_killed_worker_ends_the_asks_wait(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    join(&alpha, "lead", "claude")?;
    start(&alpha, "codex-agent", "codex", &[])?;
    ended(
        &ask_with(&alpha, "lead", ("codex-agent", "KILL_PARENT"), "10")?,
        "interrupted",
    )?;
    Ok(())
}

/// Asks to one agent run one at a time, asks to different agents run together, and repeated synchronization starts no extra worker.
fn asks_take_turns_per_agent_and_run_together_across_agents(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    setup(&alpha, &["beta"])?;
    let lead = join(&alpha, "lead", "claude")?;
    let (_, solo) = start(&alpha, "solo", "codex", &[])?;
    start(&alpha, "left", "claude", &[])?;
    start(&alpha, "right", "opencode", &[])?;
    let runs = concurrently(&[
        (&alpha, "lead", "solo", "DELAY one"),
        (&alpha, "lead", "solo", "DELAY two"),
        (&alpha, "lead", "solo", "DELAY three"),
    ])?;
    let mut answers = runs
        .iter()
        .map(|run| Ok(answered(run)?.1))
        .collect::<Result<Vec<String>, Failure>>()?;
    answers.sort();
    ensure!(
        answers
            == [
                "fake codex answer 1",
                "fake codex answer 2",
                "fake codex answer 3"
            ],
        "each ask to one agent should get its own answer: {answers:?}"
    );
    let together = concurrently(&[
        (&alpha, "lead", "left", "RENDEZVOUS"),
        (&alpha, "lead", "right", "RENDEZVOUS"),
    ])?;
    for run in &together {
        let (_, answer) = answered(run)?;
        ensure!(
            answer.ends_with("; met"),
            "asks to different agents should run at the same time: {run}"
        );
    }
    world.settle()?;
    let made = calls(&solo)?;
    ensure!(
        made.len() == 3 && made.iter().all(|call| call.get("overlap").is_none()),
        "one agent's calls should never overlap: {made:#?}"
    );
    no_worker_pileup(world, (&alpha, &beta), &lead)
}

/// While a worker is busy, new asks and repeated synchronization queue behind it instead of starting more workers.
fn no_worker_pileup(
    world: &World,
    (alpha, beta): (&Machine<'_>, &Machine<'_>),
    lead: &str,
) -> Result<(), Failure> {
    join(beta, "pinger", "codex")?;
    alpha.chat(&["sync"])?.exited(0)?;
    let hung = pending(&ask_with(alpha, "lead", ("solo", "HANG"), "0")?)?;
    let mut queued = Vec::new();
    for round in 0..3 {
        queued.push(pending(&ask_with(alpha, "lead", ("solo", "queued"), "0")?)?);
        beta.chat_as("pinger", &["send", lead, &format!("ping {round}")])?
            .exited(0)?;
        alpha.chat(&["sync"])?.exited(0)?;
    }
    let workers = world
        .processes()?
        .into_iter()
        .filter(|(_, command)| command.contains("chat-worker") && command.contains("solo@"))
        .count();
    ensure!(
        workers == 1,
        "one busy agent should have one worker, not {workers}"
    );
    world.kill_hung()?;
    ended(&alpha.chat(&["wait", &hung, "--timeout", "30"])?, "failed")?;
    for request in &queued {
        answered(&alpha.chat(&["wait", request, "--timeout", "30"])?)?;
    }
    Ok(())
}

/// An interactive agent works through the real MCP stdio server: identity, inbox, exact replies, concurrency, and cancellation.
fn mcp_serves_an_interactive_agent(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    join(&alpha, "asker", "codex")?;
    let mut mcp = alpha.mcp(&[])?;
    let initialized = mcp.request(
        1,
        "initialize",
        &json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "1"}}),
    )?;
    ensure!(
        text(&initialized, "/result/protocolVersion")? == "2025-06-18",
        "the server should accept the protocol version: {initialized}"
    );
    mcp.notify("notifications/initialized", &json!({}))?;
    let listed = mcp.request(2, "tools/list", &json!({}))?;
    let tools = labels(&listed, "/result/tools", "/name")?;
    for tool in ["chat_join", "chat_inbox", "chat_reply", "chat_ask"] {
        ensure!(
            tools.contains(&tool.to_owned()),
            "{tool} is missing: {tools:?}"
        );
    }
    let anonymous = mcp.call(3, "chat_send", &json!({"target": "asker", "text": "hi"}))?;
    ensure!(
        anonymous.get("isError") == Some(&Value::Bool(true))
            && text(&anonymous, "/content/0/text")?.contains("identity"),
        "a write without an identity should be refused: {anonymous}"
    );
    let joined = mcp.call(4, "chat_join", &json!({"name": "helper"}))?;
    ensure!(
        text(&joined, "/structuredContent/card/tool")? == "claude"
            && joined.pointer("/structuredContent/unread") == Some(&json!(0)),
        "joining should bind the session as the client's tool and report unread messages: {joined}"
    );
    let request = pending(&ask_with(&alpha, "asker", ("helper", "two plus two"), "0")?)?;
    mcp_answers(&mut mcp, &request)?;
    let waited = alpha.chat(&["wait", &request, "--timeout", "10"])?;
    let (_, answer) = answered(&waited)?;
    ensure!(
        answer == "four" && text(&waited.json()?, "/answer/from")? == "helper@local",
        "the asker should see the exact reply: {waited}"
    );
    mcp_cancels(&mut mcp)?;
    for line in mcp.finish()? {
        let message: Value = serde_json::from_str(&line).map_err(|error| {
            Failure::new(format!("stdout held a non-JSON line {line:?}: {error}"))
        })?;
        ensure!(
            message.get("jsonrpc") == Some(&json!("2.0")),
            "stdout held a line that is not JSON-RPC: {line}"
        );
    }
    ensure!(
        !crate::present(&alpha.work().join("calls.jsonl"))?,
        "no AI CLI should run for interactive agents"
    );
    Ok(())
}

/// The MCP agent sees the ask as unread, reads it from its inbox, and answers its exact ID.
fn mcp_answers(mcp: &mut crate::world::Mcp, request: &str) -> Result<(), Failure> {
    let whoami = mcp.call(5, "chat_whoami", &json!({}))?;
    ensure!(
        whoami.pointer("/structuredContent/unread") == Some(&json!(1)),
        "the ask should show as unread: {whoami}"
    );
    let inbox = mcp.call(6, "chat_inbox", &json!({}))?;
    let asks: Vec<&Value> = inbox
        .pointer("/structuredContent/events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|event| event.get("id") == Some(&json!(request)))
        .collect();
    ensure!(
        matches!(asks.as_slice(), [event] if event.get("kind") == Some(&json!("ask"))),
        "the inbox should hold the ask exactly once: {inbox}"
    );
    let replied = mcp.call(
        7,
        "chat_reply",
        &json!({"message": request, "text": "four"}),
    )?;
    ensure!(
        replied.get("isError") == Some(&Value::Bool(false)),
        "the reply should be stored: {replied}"
    );
    Ok(())
}

/// A long `chat_ask` leaves `ping` answered, and cancelling it suppresses its reply.
fn mcp_cancels(mcp: &mut crate::world::Mcp) -> Result<(), Failure> {
    mcp.start(
        8,
        "tools/call",
        &json!({"name": "chat_ask", "arguments": {"target": "asker", "text": "never answered", "timeout": 30}}),
    )?;
    let pinged = mcp.request(9, "ping", &json!({}))?;
    ensure!(
        pinged.get("result") == Some(&json!({})),
        "ping should answer during a long ask: {pinged}"
    );
    ensure!(
        mcp.response(8, 500)?.is_none(),
        "the ask should still be waiting"
    );
    mcp.notify(
        "notifications/cancelled",
        &json!({"requestId": 8, "reason": "the scenario stops waiting"}),
    )?;
    mcp.request(10, "ping", &json!({}))?;
    let late = mcp.response(8, 3000)?;
    ensure!(
        late.is_none(),
        "a cancelled call must not be answered: {late:?}"
    );
    Ok(())
}

/// Three machines discover each other, keep private messages private, survive outages and duplicates, and retry out-of-order answers.
fn three_machines_exchange_privately_and_converge(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    let gamma = world.machine("gamma");
    setup(&alpha, &["beta", "gamma"])?;
    setup(&beta, &["alpha", "gamma"])?;
    setup(&gamma, &["alpha", "beta"])?;
    join(&alpha, "ann", "claude")?;
    join(&beta, "ben", "codex")?;
    join(&gamma, "gus", "opencode")?;
    let (_, secretive) = start(&alpha, "secretive", "codex", &["--skill", "secrets"])?;
    for machine in [&alpha, &beta, &gamma] {
        machine.chat(&["sync"])?.exited(0)?;
    }
    discovery(&gamma)?;
    private_messages(&alpha, &beta, &gamma)?;
    offline_delivery(&alpha, &gamma)?;
    duplicate_delivery(world, &alpha, &beta)?;
    room_and_early_answer(world, &alpha, &beta, &gamma)?;
    answered(&ask(&alpha, "ann", "secretive", "keep this to yourself")?)?;
    for machine in [&alpha, &beta, &gamma] {
        machine.chat(&["sync"])?.exited(0)?;
    }
    secrets_stay_home(&alpha, &[&beta, &gamma], &secretive)
}

/// The directory lists every machine's agents under the aliases this machine knows them by.
fn discovery(gamma: &Machine<'_>) -> Result<(), Failure> {
    let listed = gamma.chat(&["directory"])?;
    let json = listed.exited(0)?.json()?;
    let agents = labels(&json, "/agents", "/agent")?;
    for expected in ["ann@alpha", "ben@beta", "gus@local", "secretive@alpha"] {
        ensure!(
            agents.contains(&expected.to_owned()),
            "{expected} is missing from the directory: {listed}"
        );
    }
    let machines = labels(&json, "/agents", "/machine")?;
    let systems = labels(&json, "/agents", "/os")?;
    ensure!(
        machines.contains(&"alpha".to_owned()) && systems.len() == agents.len(),
        "every agent should carry its machine and operating system: {listed}"
    );
    Ok(())
}

/// A direct message reaches its audience, and the third machine never stores its text.
fn private_messages(
    alpha: &Machine<'_>,
    beta: &Machine<'_>,
    gamma: &Machine<'_>,
) -> Result<(), Failure> {
    let secret = "private-token-for-ben-only";
    alpha
        .chat_as("ann", &["send", "ben@beta", secret])?
        .exited(0)?;
    let inbox = beta.chat_as("ben", &["inbox"])?;
    ensure!(
        inbox.exited(0)?.stdout.contains(secret),
        "the direct message should reach ben: {inbox}"
    );
    gamma.chat(&["sync"])?.exited(0)?;
    ensure!(
        !files_containing(&beta.state(), secret)?.is_empty(),
        "beta's state should hold the message, or this check cannot see a leak"
    );
    let leaked = files_containing(&gamma.state(), secret)?;
    ensure!(
        leaked.is_empty(),
        "gamma should store only a placeholder for ann's private message: {leaked:?}"
    );
    Ok(())
}

/// A message to an offline machine waits and arrives once it reconnects.
fn offline_delivery(alpha: &Machine<'_>, gamma: &Machine<'_>) -> Result<(), Failure> {
    gamma.set_offline(true)?;
    let sent = alpha.chat_as("ann", &["send", "gus@gamma", "while you were away"])?;
    sent.exited(0)?;
    ensure!(
        sent.stderr.contains("gamma not synchronized"),
        "the sender should report the unreachable peer: {sent}"
    );
    gamma.set_offline(false)?;
    let inbox = gamma.chat_as("gus", &["inbox"])?;
    ensure!(
        inbox.exited(0)?.stdout.contains("while you were away"),
        "the message should arrive after reconnecting: {inbox}"
    );
    Ok(())
}

/// Events a node stored before its reply was lost are sent again and stored once.
fn duplicate_delivery(
    world: &World,
    alpha: &Machine<'_>,
    beta: &Machine<'_>,
) -> Result<(), Failure> {
    beta.set_drop_reply(true)?;
    alpha
        .chat_as("ann", &["send", "ben@beta", "sent twice"])?
        .exited(0)?;
    beta.set_drop_reply(false)?;
    let dropped = world
        .ssh_log()?
        .iter()
        .any(|entry| entry.get("outcome") == Some(&json!("dropped")));
    ensure!(dropped, "beta's reply should have been lost once");
    alpha.chat(&["sync", "beta"])?.exited(0)?;
    let thread = beta.chat_as("ben", &["thread", "ann@alpha"])?;
    let copies = thread.exited(0)?.stdout.matches("sent twice").count();
    ensure!(
        copies == 1,
        "the resent message should be stored once, not {copies} times: {thread}"
    );
    Ok(())
}

/// A room owned by alpha changes members and topic, and an answer that reaches gamma before its question is retried within one sync.
fn room_and_early_answer(
    world: &World,
    alpha: &Machine<'_>,
    beta: &Machine<'_>,
    gamma: &Machine<'_>,
) -> Result<(), Failure> {
    alpha
        .chat_as("ann", &["room", "create", "release", "ben@beta"])?
        .exited(0)?;
    alpha
        .chat_as("ann", &["room", "add", "release", "gus@gamma"])?
        .exited(0)?;
    alpha
        .chat_as("ann", &["room", "topic", "release", "ship 1.0"])?
        .exited(0)?;
    beta.chat(&["sync"])?.exited(0)?;
    gamma.set_offline(true)?;
    let question = pending(&beta.chat_as(
        "ben",
        &[
            "ask",
            "release",
            "is it ready",
            "--to",
            "ann@alpha",
            "--timeout",
            "0",
        ],
    )?)?;
    alpha
        .chat_as("ann", &["reply", &question, "ready"])?
        .exited(0)?;
    gamma.set_offline(false)?;
    let before = world.ssh_log()?.len();
    let synced = gamma.chat(&["sync"])?;
    synced.exited(0)?;
    let calls_to_alpha = world
        .ssh_log()?
        .iter()
        .skip(before)
        .filter(|entry| {
            entry.get("from") == Some(&json!("gamma"))
                && entry.get("alias") == Some(&json!("alpha"))
        })
        .count();
    ensure!(
        calls_to_alpha >= 2,
        "gamma should retry alpha within the same sync after beta's question arrived: {synced}"
    );
    let thread = gamma.chat_as("gus", &["thread", "release"])?;
    ensure!(
        thread.exited(0)?.stdout.contains("is it ready") && thread.stdout.contains("\"ready\""),
        "gamma should hold both the question and its answer: {thread}"
    );
    alpha
        .chat_as("ann", &["room", "remove", "release", "ben@beta"])?
        .exited(0)?;
    gamma.chat(&["sync"])?.exited(0)?;
    let rooms = gamma.chat(&["room", "list"])?;
    let json = rooms.exited(0)?.json()?;
    ensure!(
        strings(&json, "/rooms/0/members")? == ["ann@alpha", "gus@local"]
            && text(&json, "/rooms/0/topic")? == "ship 1.0",
        "gamma should see the room's final members and topic: {rooms}"
    );
    Ok(())
}

/// A managed agent's session and every working directory stay on their own machine.
fn secrets_stay_home(
    alpha: &Machine<'_>,
    others: &[&Machine<'_>],
    secretive: &Path,
) -> Result<(), Failure> {
    let directory = secretive.to_string_lossy().into_owned();
    let work = alpha.work().to_string_lossy().into_owned();
    for needle in [CODEX_SESSION, directory.as_str()] {
        ensure!(
            !stored_anywhere(&alpha.state(), needle)?.is_empty(),
            "alpha should keep {needle}, or this check cannot see a leak"
        );
    }
    for machine in others {
        for needle in [CODEX_SESSION, work.as_str()] {
            let leaked = stored_anywhere(&machine.state(), needle)?;
            ensure!(
                leaked.is_empty(),
                "{needle} left alpha and reached {leaked:?}"
            );
        }
    }
    Ok(())
}

/// The files under `directory` that hold `text` as written or as JSON stores it, with escaped backslashes.
fn stored_anywhere(directory: &Path, text: &str) -> Result<Vec<PathBuf>, Failure> {
    let mut found = files_containing(directory, text)?;
    let escaped = text.replace('\\', "\\\\");
    if escaped != text {
        found.extend(files_containing(directory, &escaped)?);
    }
    Ok(found)
}

/// An answer and a withdrawal written at once on two machines end the ask the same way on both.
fn racing_answer_and_withdrawal_agree_everywhere(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    setup(&alpha, &["beta"])?;
    setup(&beta, &["alpha"])?;
    join(&alpha, "asker", "codex")?;
    join(&beta, "answerer", "claude")?;
    beta.chat(&["sync"])?.exited(0)?;
    let request = pending(&ask_with(
        &alpha,
        "asker",
        ("answerer@beta", "race me"),
        "0",
    )?)?;
    alpha.set_offline(true)?;
    beta.set_offline(true)?;
    beta.chat_as("answerer", &["reply", &request, "an answer"])?
        .exited(0)?;
    alpha.chat_as("asker", &["withdraw", &request])?.exited(0)?;
    alpha.set_offline(false)?;
    beta.set_offline(false)?;
    for machine in [&alpha, &beta, &alpha] {
        machine.chat(&["sync"])?.exited(0)?;
    }
    let endings = [&alpha, &beta]
        .iter()
        .map(|machine| {
            let json = machine.chat(&["wait", &request])?.json()?;
            Ok((
                text(&json, "/state")?.to_owned(),
                text(&json, "/ended_by")?.to_owned(),
            ))
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    ensure!(
        matches!(endings.as_slice(), [first, second] if first == second
            && matches!(first.0.as_str(), "answered" | "withdrawn")),
        "both machines should select the same ending: {endings:?}"
    );
    Ok(())
}

/// A managed agent asks another during its turn; the chain travels with the ask and refuses to wait on anyone already waiting.
fn nested_asks_carry_the_chain_and_refuse_cycles(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    join(&alpha, "alice", "claude")?;
    let (_, bob) = start(&alpha, "bob", "codex", &[])?;
    let (_, carol) = start(&alpha, "carol", "claude", &[])?;
    let asked = ask(
        &alpha,
        "alice",
        "bob",
        "DELEGATE_TO_carol DELEGATE_TO_alice",
    )?;
    let (request, answer) = answered(&asked)?;
    ensure!(
        answer.contains("asked carol, exit 0")
            && answer.contains("asked alice, exit 1")
            && answer.contains("deadlock"),
        "bob should reach carol, and carol's ask back to alice should be refused as a cycle: {answer}"
    );
    world.settle()?;
    let (bob_calls, carol_calls) = (calls(&bob)?, calls(&carol)?);
    let nested = alpha.chat_as("bob", &["thread", "carol"])?;
    let events = nested.exited(0)?.json()?;
    let nested_ask = events
        .pointer("/events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|event| event.get("kind") == Some(&json!("ask")))
        .and_then(|event| event.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::new(format!("bob's ask to carol is missing: {nested}")))?;
    ensure!(
        matches!(bob_calls.as_slice(), [call] if call.get("turn") == Some(&json!(request)))
            && matches!(carol_calls.as_slice(), [call] if call.get("turn") == Some(&json!(nested_ask))),
        "each turn should run once, carol's inside the ask bob sent from his turn: {bob_calls:#?} {carol_calls:#?}"
    );
    Ok(())
}

/// Cleaning an acknowledged conversation keeps later syncs whole, and a reset machine is accepted again only after `peer replace`.
fn cleaned_and_reset_machines_keep_syncing(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    let gamma = world.machine("gamma");
    setup(&alpha, &["beta"])?;
    setup(&beta, &["alpha"])?;
    join(&alpha, "ann", "claude")?;
    join(&beta, "ben", "codex")?;
    beta.chat(&["sync"])?.exited(0)?;
    for message in ["one", "two"] {
        alpha
            .chat_as("ann", &["send", "ben@beta", message])?
            .exited(0)?;
    }
    beta.chat_as("ben", &["send", "ann@alpha", "three"])?
        .exited(0)?;
    alpha.chat(&["sync"])?.exited(0)?;
    let conversation = {
        let thread = alpha.chat_as("ann", &["thread", "ben@beta"])?;
        text(&thread.exited(0)?.json()?, "/events/0/conversation")?.to_owned()
    };
    let cleaned = alpha.chat(&["clean", &conversation])?;
    ensure!(
        cleaned
            .exited(0)?
            .json()?
            .get("cleaned")
            .and_then(Value::as_u64)
            == Some(3),
        "the acknowledged conversation should be cleaned: {cleaned}"
    );
    let emptied = alpha.chat_as("ann", &["thread", &conversation])?;
    ensure!(
        emptied.exited(0)?.json()?.pointer("/events") == Some(&json!([])),
        "alpha should no longer hold the conversation: {emptied}"
    );
    alpha
        .chat_as("ann", &["send", "ben@beta", "after the clean"])?
        .exited(0)?;
    let kept = beta.chat_as("ben", &["thread", "ann@alpha"])?;
    ensure!(
        kept.exited(0)?.stdout.contains("\"one\"") && kept.stdout.contains("after the clean"),
        "beta should keep its copy and receive later messages: {kept}"
    );
    later_peer(&alpha, &gamma)?;
    reset_and_replace(&alpha, &beta)
}

/// A peer pinned after the clean receives alpha's whole sequence, with placeholders for the cleaned part.
fn later_peer(alpha: &Machine<'_>, gamma: &Machine<'_>) -> Result<(), Failure> {
    setup(gamma, &["alpha"])?;
    join(gamma, "gus", "opencode")?;
    setup(alpha, &["gamma"])?;
    gamma.chat(&["sync"])?.exited(0)?;
    alpha
        .chat_as("ann", &["send", "gus@gamma", "hello, later peer"])?
        .exited(0)?;
    let inbox = gamma.chat_as("gus", &["inbox"])?;
    ensure!(
        inbox.exited(0)?.stdout.contains("hello, later peer"),
        "a peer pinned after the clean should keep synchronizing: {inbox}"
    );
    Ok(())
}

/// A machine that reset its chat is refused until the other side confirms its new identity.
fn reset_and_replace(alpha: &Machine<'_>, beta: &Machine<'_>) -> Result<(), Failure> {
    let reset = beta.chat(&["reset", "--yes"])?;
    let origin = text(&reset.exited(0)?.json()?, "/origin")?.to_owned();
    let refused = alpha.chat(&["sync", "beta"])?;
    ensure!(
        refused.code == Some(1) && refused.stdout.contains("peer replace"),
        "a reset peer should be refused until it is replaced: {refused}"
    );
    let replaced = alpha.chat(&["peer", "replace", "beta"])?;
    ensure!(
        text(&replaced.exited(0)?.json()?, "/origin")? == origin,
        "replacing should pin the new identity: {replaced}"
    );
    alpha.chat(&["sync", "beta"])?.exited(0)?;
    Ok(())
}

/// `chat serve`, run as a plain child process, pulls a peer's messages without any command and catches up after an outage.
fn background_service_delivers_without_commands(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    setup(&alpha, &["beta"])?;
    setup(&beta, &["alpha"])?;
    join(&alpha, "ann", "claude")?;
    join(&beta, "ben", "codex")?;
    alpha.chat(&["sync"])?.exited(0)?;
    alpha.set_offline(true)?;
    alpha.start_background(&["chat", "serve"])?;
    let pid_file = alpha.state().join("v1").join("chat").join("serve.pid");
    wait_for("the chat service to start", || {
        Ok(crate::present(&pid_file)?.then_some(()))
    })?;
    beta.chat_as("ben", &["send", "ann@alpha", "delivered by the service"])?
        .exited(0)?;
    arrives(&alpha, "delivered by the service")?;
    beta.set_offline(true)?;
    beta.chat_as("ben", &["send", "ann@alpha", "sent during the outage"])?
        .exited(0)?;
    crate::pause(2000);
    beta.set_offline(false)?;
    arrives(&alpha, "sent during the outage")?;
    world.stop_background()?;
    Ok(())
}

/// Waits until alpha's copy of the conversation with ben holds `text`, without alpha synchronizing itself.
fn arrives(alpha: &Machine<'_>, text: &str) -> Result<(), Failure> {
    wait_within("the service to deliver the message", 900, || {
        let thread = alpha.chat_as("ann", &["thread", "ben@beta"])?;
        Ok(thread.exited(0)?.stdout.contains(text).then_some(()))
    })
}

/// `domyjob on` and `domyjob run` still reach a job through the node wrapper and pass its exit status back.
fn jobs_run_through_the_node_wrapper(world: &World) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
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
    Ok(())
}

/// Pins `peers` on `machine` without installing the service or registering AI clients.
fn setup(machine: &Machine<'_>, peers: &[&str]) -> Result<(), Failure> {
    let mut arguments = vec!["setup"];
    arguments.extend_from_slice(peers);
    arguments.extend(["--no-service", "--no-clients"]);
    machine.chat(&arguments)?.exited(0)?;
    Ok(())
}

/// Registers an interactive agent and returns its ID, `NAME@ORIGIN`.
fn join(machine: &Machine<'_>, name: &str, tool: &str) -> Result<String, Failure> {
    let joined = machine.chat(&["agent", "join", name, "--tool", tool])?;
    Ok(text(&joined.exited(0)?.json()?, "/agent")?.to_owned())
}

/// Registers a managed agent working in `work/NAME` and returns its ID and working directory.
fn start(
    machine: &Machine<'_>,
    name: &str,
    tool: &str,
    profile: &[&str],
) -> Result<(String, PathBuf), Failure> {
    let directory = machine.workdir(name)?;
    let cwd = directory
        .to_str()
        .ok_or_else(|| Failure::new(format!("{} is not UTF-8", directory.display())))?;
    let mut arguments = vec!["agent", "start", name, "--tool", tool, "--cwd", cwd];
    arguments.extend_from_slice(profile);
    let started = machine.chat(&arguments)?;
    Ok((
        text(&started.exited(0)?.json()?, "/agent")?.to_owned(),
        directory,
    ))
}

fn ask(machine: &Machine<'_>, asker: &str, target: &str, question: &str) -> Result<Run, Failure> {
    ask_with(machine, asker, (target, question), "30")
}

/// Asks `(target, question)` as `asker`, waiting at most `timeout` seconds.
fn ask_with(
    machine: &Machine<'_>,
    asker: &str,
    (target, question): (&str, &str),
    timeout: &str,
) -> Result<Run, Failure> {
    machine.chat_as(asker, &["ask", target, question, "--timeout", timeout])
}

/// Runs several asks at once, each as `(machine, asker, target, question)`.
fn concurrently(asks: &[(&Machine<'_>, &str, &str, &str)]) -> Result<Vec<Run>, Failure> {
    std::thread::scope(|scope| {
        // Every ask starts before the first one is awaited.
        let mut running = Vec::new();
        for (machine, asker, target, question) in asks {
            running.push(scope.spawn(move || ask(machine, asker, target, question)));
        }
        running
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_panic| Failure::new("an ask thread panicked"))?
            })
            .collect()
    })
}

/// Checks that an ask was answered, and returns its message ID and the answer's text.
fn answered(run: &Run) -> Result<(String, String), Failure> {
    let json = run.exited(0)?.json()?;
    let id = text(&json, "/message_id")?;
    ensure!(
        text(&json, "/state")? == "answered"
            && text(&json, "/answer/kind")? == "reply"
            && text(&json, "/answer/request")? == id,
        "expected an answer to {id}: {run}"
    );
    Ok((id.to_owned(), text(&json, "/answer/text")?.to_owned()))
}

/// Checks that an ask ended as `state` without an answer, and returns its message ID.
fn ended(run: &Run, state: &str) -> Result<String, Failure> {
    let json = run.exited(1)?.json()?;
    ensure!(
        text(&json, "/state")? == state && json.get("answer").is_none(),
        "expected the ask to end {state}: {run}"
    );
    Ok(text(&json, "/message_id")?.to_owned())
}

/// Checks that an ask is still waiting for its responder, and returns its message ID.
fn pending(run: &Run) -> Result<String, Failure> {
    let json = run.exited(3)?.json()?;
    ensure!(
        matches!(text(&json, "/state")?, "pending" | "working"),
        "expected the ask to wait: {run}"
    );
    Ok(text(&json, "/message_id")?.to_owned())
}

/// The text at `field` of every element of the list at `list`.
fn labels(value: &Value, list: &str, field: &str) -> Result<Vec<String>, Failure> {
    value
        .pointer(list)
        .and_then(Value::as_array)
        .ok_or_else(|| Failure::new(format!("no list at {list} in {value:#}")))?
        .iter()
        .map(|item| Ok(text(item, field)?.to_owned()))
        .collect()
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
