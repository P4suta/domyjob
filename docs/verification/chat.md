# Chat verification

This records how the agent chat was verified on 2026-09-29, what the checks found, and what remains outside them.

## Machines

| Alias | System | Role |
| --- | --- | --- |
| (local) | macOS on Apple silicon | Hub: runs setup, the launchd service, and the MCP clients |
| `linux` | Ubuntu on x86-64 | Peer with managed Claude Code, Codex, and OpenCode agents |
| `win` | Windows on x86-64 | Peer with managed Claude Code and Codex agents |

## Mechanical checks

- `mise run lint`: formatting, Clippy for the host, Linux, and Windows targets, the gate, spelling, workflows, and duplication.
- `mise run test` on macOS, and `mise run check:linux` and `mise run check:win` on the other machines:
  unit tests, the format specimens, the search of every reachable state of a room ask and a direct ask across three machines (about 8,700 states each), and the twelve end-to-end scenarios with fake SSH and fake AI clients.
- `mise run deny`, `audit`, and `vet` pass; `redb` is still an exempted crate in `cargo vet`, with no published audit.
- The model search was shown to fail at once when the order of endings is reversed.

## Real machines

| Check | Result |
| --- | --- |
| `chat setup linux win` | Pinned both peers, installed each node once, installed the launchd service, and registered the MCP server with Claude Code, Codex, and OpenCode |
| Services | Installed and running under launchd on macOS, a systemd user unit on Linux, and a logon task on Windows |
| Delivery to the hub | A message written on Linux reached the Mac within a second, with no command run on the Mac |
| Push from the hub | A write on the Mac reached Linux within 0.33 to 0.36 seconds, three times |
| Reconnection | After the service's long-poll connection was killed, it reconnected, and the next message arrived at once |
| Managed turns | Codex and Claude Code answered on Linux and Windows, and OpenCode on Linux; the final build repeated Codex on Windows and Claude Code and Codex on Linux |
| Consultation | Claude Code on Linux used its per-turn MCP server to ask Codex on Linux, and relayed the answer |

OpenCode on Windows is not signed in on that machine, and `chat doctor` there says so; its turns were not verified.

Afterwards `chat reset` replaced each machine's chat identity and removed its service.

## Found while verifying

Each of these was fixed with a test that fails without the fix.

- A worker killed during a turn left its ask working until another dispatch; waits now drive progress.
- A job that exited at once could be recorded as lost, because stopping its already finished output relay failed.
- The Linux disk filled because every node install and fleet check left a full set of build artifacts; they are now cleaned.
- Finished jobs accumulated without bound; each admission keeps the newest 32.
- A tool that printed more than 1 MiB was reported as failed; its output is now drained.
- FSEvents woke the launchd service late or never, so the hub pushed writes once a minute; waits now use kqueue on macOS.
- `powershell.exe` was searched for as `powershell.exe.exe`, so the Windows service could not be installed.
- A chat store without a recorded format was accepted, and reset, doctor, and the service commands could not handle a store they could not open.
- Reset stopped the service only when it ran the current build, so a service of an older build kept running against the replaced store.
- Setup rewrote the OpenCode configuration as a new private file, which changed its mode and would have replaced a symbolic link with a copy.
- A finished job holding files this user cannot delete, as a container's files owned by root are, would have stopped every later job on that machine.
- On Windows, replacing a state file failed while another process read it, which mutation testing's load exposed in the doorbell;
  files are now replaced by the standard library's rename, which Windows performs even then.
- Two tests passed only with lucky timing on CI:
  a lock that one test released could still be held by a child that another test forked,
  and Windows could still hold an end-to-end world's files after its processes ended.

## Outside these checks

- State written by the former implementation is not read or migrated,
  and on the Linux machine it occupies about 46 GB under `~/.local/state/domyjob` and `~/.cache/domyjob`.
- A fleet of machines that do not all reach one another is supported only as the star seen here, where the hub reaches every peer.
