---
name: domyjob
description: Run builds, tests, or any command on another machine or operating system (Linux, Windows, macOS) straight from the current directory, uncommitted edits included, and read the outcome in a few lines. Use it when work has to happen on a different OS or machine, needs more power than this one, or should keep running after you stop.
---

# domyjob

domyjob sends the current directory to other machines, runs a command there, and keeps it running whether or not you stay.
Run these commands where you work: machines that only receive jobs need no `domyjob` on their PATH.
You then ask for a digest or search the log instead of reading all of it.

## The loop

1. `domyjob machines` shows where you can run and each machine's labels, such as `os=windows`.
2. Start the work without waiting, and give it a name:
   `domyjob run win --name tests -- cargo test` It returns at once with a job reference; the job keeps running on its own.
3. Learn how it went, cheaply:
   `domyjob digest tests` prints the state, the exit code, how long the log is, and its last 40 lines.
4. Dig only where it matters:
   `domyjob logs tests --grep 'panicked|error\[' --context 3`
5. Fetch a result if you need one:
   `domyjob get tests target/report.json -o report.json`
6. Bring back what it changed:
   `domyjob pull tests` writes the files the job added, altered, or removed into the directory it was sent from, and refuses if you edited any of them since sending.
   `domyjob pull --undo tests` puts them back.
   Commit, sign, and push here: the other machines never need your keys.

When a result is needed right away and the job is short, `domyjob run win --wait --digest -- cargo test` waits and prints only the digest.
To watch it live but only see what matters, `--wait --grep 'test result|panicked'` shows just the matching lines while the whole log stays on the machine.
When it may be long, start it without waiting and use `domyjob wait tests` or `domyjob digest tests` later: stopping `wait`, or being cut off by a timeout of your own, never stops the job.

## Jobs a project names

A `domyjob.toml` at the project root names jobs so nobody retypes them:

```toml
[jobs.test]
on = "@all"
run = ["cargo", "test"]
```

`domyjob do test` runs it where `on` says, `--on win` runs it elsewhere, and `domyjob do test @main` sends that revision instead of the directory on disk.
Each job may also set `runner`, `workspace` (`warm` or `fresh`), `dir`, and `env`.

## Looking at machines

`domyjob` with no command asks every machine at once and prints one card each: its load, memory, and free disk, what runs and waits there, the last failure, and the one command that deals with whatever needs attention.
`domyjob --json` gives the same as data, and `domyjob --live` keeps it on screen, redrawn whenever a job starts or finishes (with `--json`, one line per change).
`domyjob clean MACHINES` frees the disk domyjob holds there (`--dry-run` first shows how much), `domyjob machines pause MACHINES` stops them taking new jobs until `resume`, and `domyjob machines limit MACHINES 8` lets eight run at once.

## Looking at a machine, instead of ssh

`domyjob on win -- Get-ChildItem Downloads` runs a command on a machine right now and prints only its output, with the command's own exit code.
Nothing is sent and it runs in the machine's home directory, so use it wherever you would have typed `ssh MACHINE COMMAND`: checking disk space, listing files, reading a config.
`domyjob on @all -- uptime` asks every machine at once.

## Referring to jobs

- By the name you gave with `--name`: `tests`, or `win:tests` for the one on that machine.
- The newest job you started: `latest`, or `win:latest`.
- Names and `latest` mean jobs sent from the project you are in, so two worktrees never answer for each other; an id works from anywhere.
- By id or any unique prefix of it: `3KX9`, or `win:3KX9`.

## Choosing machines

`MACHINES` is a name, labels joined with `+` such as `windows+gpu`, a fact such as `os=linux`, `@group`, or `@all`, separated by commas.
`domyjob run @all --wait --digest -- cargo test` checks every operating system at once and prints one digest per machine.

## What differs on the other side

- The command runs where the files landed: the same subdirectory you are in, inside a workspace that keeps build output between runs, so the second build is incremental.
  Every worktree of one repository shares that workspace on each machine, so a new worktree builds incrementally too; `--fresh` is only for when you need an empty directory.
- Windows runs the command with PowerShell unless you pass `--shell`; write PowerShell there, not sh.
- A single argument after `--` is a script for that shell; several arguments run as a program and its arguments without any shell.
- Jobs keep the environment of the ssh session that started them, but anything that lived only in that session, such as a forwarded ssh agent, is gone once it closes.
  Clone private repositories with credentials the machine itself holds.
- `.gitignore`, `.ignore`, and `.domyjobignore` decide what is sent; everything else goes, whether or not it is committed.
- Tools that ask before trusting a directory, such as mise or direnv, see each workspace as a new one: trust domyjob's work area once in that machine's own settings.

## Exit codes

- `0`: everything asked for succeeded.
- `1`: domyjob worked, and a job failed, or `status` found a failed job, or `logs --grep` found nothing, or `doctor` found a problem.
  `run --wait` on a single machine exits with the job's own exit code instead, so it can stand in for running the command there directly.
- `2`: domyjob itself could not do what was asked; with `--json` the error arrives as `{"error": {"kind": ..., "message": ..., "hint": ...}}`, where `kind` is one of `usage`, `config`, `not_found`, `ambiguous`, `unreachable`, `forbidden`, `protocol`, `security`, `distribution`, `local`, `remote`, and `internal`.
- `3`: the outcome is unknown: a machine did not answer, or a job was lost track of. The job may still be running; ask again rather than sending it again.

## When the network or a machine misbehaves

- A job keeps running when your connection drops; ask again with `domyjob status JOB` or `domyjob wait JOB`.
- A machine that stops answering is reported as "went silent"; `domyjob doctor` says what to try.
- If you reset or reinstalled a machine yourself and domyjob reports its audit log changed, `domyjob machines rewitness NAME --accept` accepts it.

## JSON for scripts and agents

Every line of `--json` output, and every MCP answer, is one document with `"schema": 3`.
A job has `job` (`machine:id`), `machine`, `state`, `exit_code`, `name`, `command`, `reason` (why it errored, if it did), `behind` (the jobs a queued job is waiting for), `notes` (what domyjob itself had to say about the job, kept out of its log), and from the command line `detail` (everything else).
`digest` adds `lines`, `bytes`, and `tail` to the job.
`ls --json` answers `{"jobs": [...], "unreachable": [...]}`, as the MCP `list_jobs` does.
Wherever machines are listed, one that could not answer is `{"machine": ..., "error": {kind, message, hint}}`, and a command that failed answers `{"error": {kind, message, hint}}`.

## Other commands

- `domyjob ls` lists jobs on every machine; `domyjob status JOB` shows one.
- `domyjob history` shows each kind of job's recent outcomes, success rate, and typical duration, to tell a flaky job from a broken one.
- `domyjob kill JOB` stops a job and everything it started, at once.
- `domyjob logs JOB -f` follows a running job's output; prefer `digest` and `--grep`, which cost far fewer tokens.
- Add `--json` to `run`, `digest`, `status`, `wait`, `ls`, and `logs --grep` for machine-readable output.
- `domyjob mcp` serves the same operations as MCP tools, if you prefer tools to commands.
- `domyjob doctor` checks that every machine answers and runs a matching domyjob, and `domyjob setup MACHINES` installs the matching one where it does not; `domyjob run --dry-run ...` shows what would be sent where without sending it.
- `domyjob machines remove NAME --wipe` removes everything domyjob placed on a machine (its jobs, workspaces, key, service, and copy of domyjob) and then forgets it; it refuses while jobs still run there unless `--kill-running` is given.
  `domyjob self uninstall` does the same for the machine you are on.
