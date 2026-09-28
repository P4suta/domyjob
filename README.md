# domyjob

Run persistent jobs on machines reachable through OpenSSH.
The client sends a command to a remote node, records its state, and keeps it running after the SSH connection ends.

## Develop from this checkout

Rust 1.98 and `mise` are pinned in this repository.
Build the client with `mise x -- cargo build --locked -p domyjob`, then run `target/debug/domyjob`.
The client compares the complete source fingerprint with its compiled fingerprint and rebuilds itself when the checkout changes.
Before each remote request, it compares the remote fingerprint and builds and installs the matching node over SSH when needed.
The binary embeds its build source, so an installed copy can perform the first remote installation without its checkout.
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

Run `mise run lint` and `mise run test` before a change is reviewed.
Use `mise run check:fleet` to run the same checks on Linux and Windows through domyjob.

Licensed under Apache-2.0 or MIT, at your option.
