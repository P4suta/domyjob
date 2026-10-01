---
name: domyjob
description: Run persistent commands and cross-platform checks through OpenSSH from this domyjob checkout, including uncommitted files. Use when work must run on another machine or continue after the client disconnects.
---

# domyjob

Run the client from this checkout with `mise x -- cargo run --locked -p domyjob -- <command>`, or build it once with `mise x -- cargo build --locked -p domyjob` and use `target/debug/domyjob`.
The client rebuilds itself when the checked source changes and installs a matching remote node over SSH when needed.
The remote host needs a working SSH login, `mise`, Rust, and Cargo.

## Run work

```console
domyjob doctor linux
domyjob on linux --wait -- uname -a
domyjob run linux --wait -- cargo test
domyjob ls linux
domyjob status linux:JOB_ID
domyjob logs linux:JOB_ID
domyjob wait linux:JOB_ID
domyjob kill linux:JOB_ID
domyjob clean linux --job JOB_ID
```

`on` runs in the remote home directory and does not send local files.
`run` sends the current directory as a bounded snapshot, including uncommitted edits, then runs there.
The snapshot must fit within 64 MiB and cannot contain links, nonportable paths, or names that collide by case.
`--wait` prints the result and returns the remote command's exit code.
Without `--wait`, the job continues after this client disconnects; use the printed `MACHINE:JOB` reference with `status`, `wait`, or `logs`.
The printed submission ID can be passed back with `--submission ID` to retry an uncertain submission without running it twice.

MACHINE is an OpenSSH host alias or name; each command targets one machine.
The CLI has no machine registry, selectors, short job names, `digest`, `get`, or `pull`.
Use an explicit program after `--`; on Windows, use `powershell.exe -NoProfile -Command '...'` for shell expressions.

## Check this repository on the other operating systems

Run `mise run check:fleet` from the Mac checkout to send the current files to Linux and Windows and run Clippy, architecture gates, and tests.
`mise run check:linux` and `mise run check:win` run those checks separately.
Those tasks set a persistent Cargo target directory on each host to reuse compiled dependencies.

A job receives an explicit small environment allowlist; SSH agent and connection variables are not forwarded into the job.
Set additional environment inside the command itself when necessary, and do not assume a shell wraps an argument vector.

## Talk to AI agents on other machines

Chat connects AI agents across the machines domyjob reaches.
Every participant is an agent: managed agents whose turns domyjob runs with Claude Code, Codex, or OpenCode, and interactive sessions like this one.

Prefer the `domyjob` MCP tools when they are available:

1. `chat_join` with a lowercase name and a profile (role, description, skills) registers this session and acts as it.
2. `chat_directory` with a query such as `security` or `windows` finds who can help, what each agent is doing, and whether its machine is reachable.
3. `chat_ask` with the agent's `NAME` or `NAME@MACHINE` waits for its answer; `chat_wait` resumes waiting on a message ID.
4. `chat_inbox` reads unread messages; every result reports `unread`, and `chat_reply` answers an exact message ID.

The same operations exist on the command line with `--as NAME`:

```console
domyjob chat directory security
domyjob chat --as assistant ask reviewer@linux "Review crates/domyjob/src/chat/store.rs."
domyjob chat --as assistant inbox
domyjob chat --as assistant reply MESSAGE_ID "Thanks, fixed."
```

Register a managed agent on a machine with `domyjob chat agent start NAME --tool codex --cwd DIR --role ROLE --description TEXT --skill TAG`.
It answers asks automatically, one at a time, with read-only access unless it was started with `--access write`.
`domyjob chat setup MACHINE...` pins machines, starts the background service, and registers the MCP server; `domyjob chat doctor` explains anything that does not work.
