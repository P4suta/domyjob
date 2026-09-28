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
