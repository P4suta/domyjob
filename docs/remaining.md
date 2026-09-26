# Remaining work

What is left, as of the crash-matrix branch.
Each item says what is wrong or missing, and what the fix should look like.
Delete an item in the change that finishes it.

## Jobs and the queue

- A job whose supervisor is gone before its command started (Queued or Preparing) reads as `lost`, and exit 3, until a command reaches the machine.
  Only commands run upkeep, because no query may change state, so `status` and `wait` never start it again.
  This now also covers a job the machine stopped before it started.
  Decide between a distinct state such as "waiting to be started again" and letting `wait` count as a command.
- A supervisor that dies between spawning the command and recording Running leaves the phase at Preparing, and recovery starts it again, so the command may run twice.
  Record that the command is about to start before spawning it, and treat that record as Running on recovery.
- On Windows the supervisor is a `DETACHED_PROCESS` with no console, so it never hears a shutdown and a restart still reads as a vanished supervisor.
- The message for a vanished supervisor still guesses "the machine may have restarted".
  An exact boot identity would make it a fact: `/proc/sys/kernel/random/boot_id` on Linux, `kern.bootsessionuuid` on macOS; Windows has no clean equivalent yet.
  Time must not decide it.
- The job limit is copied into each job when it is accepted, so lowering it does not affect jobs already queued.
  Reading the machine's settings at dequeue time would make the limit one source.
- Pause refuses new jobs only; jobs already queued still start.
  Decide whether pause should also hold the queue, in the one dequeue path and in recovery.

## Crash matrix

`node::tests::a_crash_at_any_step_*` stop the node before every step that passes through `faults::at` and check that the next run recovers.
Not covered yet:

- Workspace writes (`tree.rs`, cap-std) and user files (`user_files.rs`, `durable.rs`) do not pass through `faults::at`, apart from `pull::swap`, so a job that sends a snapshot, and most of `pull`, are not crashed.
- Client state: `index.jsonl`, `trust.json`, witnesses, the pull journal.
- A real crash of a real process: a binary-level harness that runs `domyjob node` and a supervisor under a temporary state directory, kills them with SIGKILL (or a failpoint that aborts) at each step, and checks recovery.
  The same harness would give real end-to-end job tests.

## One path for every OS

`NOT_YET_ONE_PATH` in `xtask/src/lib.rs` still allows: keystore 5, proc 5, service 6, spawn 1, xtask release 1.

- service.rs: express each service manager as data in `builtin.toml` (a file to write, commands to run on install and uninstall, per OS through `run_macos`/`run_windows`), with one executor.
- keystore.rs: consider the `keyring` crate; a headless Linux still needs the owner-only file.
- proc.rs: the remaining branches are process groups against job objects, polling a pipe, and the Windows launch through WMI.
  `process-wrap` covers groups and job objects but not the reaper or the WMI launch; weigh it against the risk.

## Shapes and inputs

- MCP tool input schemas are hand-written JSON beside the argument structs in `mcp.rs`.
  Derive them from the structs with schemars (`#[schemars(description = ...)]`), and give bounded numbers such as `tail` a type that enforces the bound.
- Jobs inherit the environment of the ssh session that started the node (`LaunchEnv::of_this_process`).
  Clear it and pass an allowlist, and add a canary variable that CI checks never reaches a job.
- The `fail` crate's failpoints are compiled into release builds; put them behind a cargo feature.

## Duplication

jscpd's threshold is 1.46%.
Groups still duplicated: running a process and reading its status and output, retry loops, directory listings, digests, lock naming, submit then wait then notify, hex and random and versions, name coercion, test fixtures, the remote layout.

## Operations

- A build stamp, so the binary a fleet check tested is the one that ships.
- A connection budget per machine; growth of the audit log; retention of old binaries under `bin/`.
- CI runs its steps directly; move them onto the mise tasks the fleet check uses.
- One push failed on linux under heavy load and was never explained.

## Experience

- `--live` was reported to misbehave in the scrollback; reproduce it before fixing.
- First run, grouping in `--help`, a `--quiet`, and guidance for pipelines that should fail (pipefail).
- Say in the documentation that letting a peer submit jobs is letting it run commands as you.

## Branches

- `wip/linger` reports whether jobs outlive the last login on Linux.
  It was written for a failure that turned out to be a reboot, and linux has `KillUserProcesses=no`, so it is not needed; delete it or keep it as a reference.
