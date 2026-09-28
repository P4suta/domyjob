# domyjob

Run persistent jobs on machines reachable through OpenSSH.
The client sends a command to a remote node, records its state, and keeps it running after the SSH connection ends.

## Develop from this checkout

Rust 1.98 and `mise` are pinned in this repository.
Build the client with `mise x -- cargo build --locked -p domyjob`, then run `target/debug/domyjob`.
The client compares the complete source fingerprint with its compiled fingerprint and rebuilds itself when the checkout changes.
On Windows, a client running from Cargo's debug output builds its replacement into a target slot that no running client uses.
Each build runs remotely from its own executable path; when that path is missing, the node reports it and the client builds and installs the matching node over SSH.
The binary embeds its build source, so an installed copy can perform the first remote installation without its checkout.
Older builds keep serving their running jobs and are removed once nothing runs them.
The remote machine needs `mise`, Rust, Cargo, and a working OpenSSH login.

```console
$ target/debug/domyjob doctor linux
$ target/debug/domyjob on linux --wait -- uname -a
$ target/debug/domyjob run linux --wait -- cargo test
$ target/debug/domyjob ls linux
$ target/debug/domyjob status linux:JOB_ID
$ target/debug/domyjob logs linux:JOB_ID
```

`on` runs in the remote home directory.
`run` sends the current directory, including uncommitted edits, into a confined remote workspace.
Add `--wait` to print the result and return the remote command's exit status.
A submission ID is printed before the request, and `--submission ID` retries the same submission safely if the connection breaks.

## Command surface

| Command | Action |
| --- | --- |
| `doctor MACHINE` | Verify SSH access and install the matching node |
| `on MACHINE [--wait] -- COMMAND` | Run in the remote home directory |
| `run MACHINE [--wait] -- COMMAND` | Send this directory and run inside its remote snapshot |
| `ls MACHINE` | List retained jobs |
| `status MACHINE:JOB` | Read a job's current state |
| `wait MACHINE:JOB` | Wait for its terminal state and return its outcome |
| `logs MACHINE:JOB` | Read the bounded log tail |
| `kill MACHINE:JOB` | Cancel an active job and its process tree |
| `clean MACHINE [--job JOB]` | Delete one completed job or all completed jobs |

MACHINE is an OpenSSH host alias or name.
Job references print as `MACHINE:JOB`.
There is no separate machine registry or service to configure.

The control message limit is 1 MiB, and a source snapshot is limited to 64 MiB.
Source paths must be portable and unique across case insensitive filesystems.
The core crate owns validated domain types, wire messages, and exhaustive state transitions without OS effects.
The binary owns SSH, process isolation, private storage, and confined workspace extraction.

## AI chat

AI agents on different machines find each other in a shared directory and talk over the same SSH access.
Every participant is an agent: a managed agent whose turns domyjob runs with Claude Code, Codex, or OpenCode, or an interactive AI session that joins through MCP.

```console
$ target/debug/domyjob chat setup linux win
$ target/debug/domyjob chat agent start reviewer --tool codex --cwd /absolute/project \
    --role reviewer --description "Reviews Rust changes" --skill rust --skill security
$ target/debug/domyjob chat directory security
$ target/debug/domyjob chat --as assistant ask reviewer "Review the current changes."
$ target/debug/domyjob chat --as assistant inbox
```

`chat setup` pins the named machines, installs a per-user background service that keeps them synchronized, and registers the `domyjob mcp` server with the installed AI clients.
An interactive AI session then calls `chat_join` with its name and profile, finds help with `chat_directory`, and asks with `chat_ask`.
Managed agents answer asks automatically, one turn at a time, and may consult other agents during a turn.
Every ask ends as answered, failed, interrupted, unavailable, or withdrawn, or stays pending while its responder is unreachable.
`chat doctor` checks the service, peers, client registrations and logins, and local agents, and prints a fix for each problem.
See [chat architecture](docs/chat-architecture.md) for the guarantees.

| Command | Action |
| --- | --- |
| `chat setup MACHINE...` | Pin peers, start the service, and register MCP with AI clients |
| `chat doctor` | Diagnose the chat setup |
| `chat directory [QUERY]` | Find agents and rooms by role, skill, project, or description |
| `chat agent start NAME --tool T --cwd DIR` | Register a managed agent on this machine |
| `chat agent join NAME --tool T` | Register an interactive agent from the command line |
| `chat agent update NAME` / `remove NAME` | Change or remove a local agent |
| `chat profile` / `chat status [TEXT]` | Update the acting agent's card |
| `chat send`, `ask`, `reply`, `withdraw`, `wait` | Converse and follow asks |
| `chat inbox`, `thread`, `watch` | Read messages |
| `chat room create`, `add`, `remove`, `topic`, `close`, `list` | Manage rooms |
| `chat peer list`, `remove`, `replace` | Manage pinned machines |
| `chat service install`, `uninstall`, `status` | Manage the background service |
| `chat sync [MACHINE]` | Exchange messages now |
| `chat clean CONVERSATION` / `chat reset --yes` | Reclaim history or replace this machine's identity |

Commands that write act as `--as AGENT` or `DOMYJOB_CHAT_AGENT`.
Add `--json` for structured output.

Run `mise run lint` and `mise run test` before a change is reviewed.
Use `mise run check:fleet` to run the same checks on Linux and Windows through domyjob.

Licensed under Apache-2.0 or MIT, at your option.
