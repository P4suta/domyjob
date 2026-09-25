# domyjob

Send your work to any machine you can reach, run it there, and walk away.

```console
$ domyjob
$ domyjob run linux,win -- cargo test
$ domyjob ls
$ domyjob digest win:latest
$ domyjob on win -- Get-ChildItem Downloads
```

It sends the directory as it is, uncommitted edits included, and the command keeps running there after you disconnect.
Nothing to host: one binary on your machine, and any host you can ssh into.

## Install

```console
$ cargo install --git https://github.com/P4suta/domyjob domyjob
```

Any host your ssh config knows works as it is; `domyjob doctor` checks them, and the first contact installs domyjob there.

## Use

| Command | Does |
| --- | --- |
| *(none)* | Every machine at a glance: load, memory, disk, what runs, what failed, and what to do next |
| `run MACHINES -- CMD` | Send this directory and run a job; `--wait` stays for the result |
| `on MACHINES -- CMD` | Run a command right now and print its output, like ssh |
| `ls`, `status`, `digest`, `logs` | See jobs, their outcome, and their output |
| `history` | How each kind of job has gone lately: outcomes, success rate, typical duration |
| `wait`, `kill`, `get` | Wait for a job, stop it, fetch a file from its workspace |
| `pull JOB` | Bring the files a finished job changed back here, to commit and push with your own keys |
| `machines`, `setup`, `doctor` | Name machines, install domyjob on them, check them |
| `clean`, `machines pause` | Free the disk space domyjob holds; stop a machine taking jobs for maintenance |

`MACHINES` is a name, `@all`, a label such as `gpu`, or a fact such as `os=windows`.

## For agents

`domyjob skill install` teaches Claude Code to use it, `domyjob mcp` serves the same operations as MCP tools, and `--json` gives one stable shape.

`domyjob --help` and `domyjob COMMAND --help` explain the rest; `domyjob man` prints the manual page.

Licensed under Apache-2.0 or MIT, at your option.
