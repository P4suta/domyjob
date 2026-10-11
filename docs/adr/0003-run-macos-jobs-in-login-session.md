# Run macOS jobs in the login session

Status: Accepted.

## Context

An SSH session on macOS cannot access the desktop login keychain even when it belongs to the same user.
Moving that session with `launchctl asuser` requires root.
The existing per-user `dev.domyjob.chat` LaunchAgent already runs in the GUI login domain.
Jobs must retain their environment allowlist, build-specific store, workspace, output, cancellation, and process-tree supervision without persisting credentials.

## Decision

The chat LaunchAgent hosts the [login handoff adapter](../../crates/domyjob/src/process/login.rs) only when its launchctl manager is Aqua.
SSH nodes check the user's GUI domain and send a bounded request over a Unix socket in an owner-only directory.
The socket directory is shared across builds for the same user home and has a short fixed path to respect the native socket-path limit.
An OS lock gives one service ownership and permits stale socket replacement after that service exits.

The request carries the node's exact executable, validated job ID, absolute state root, and the existing environment allowlist.
The service validates paths and environment names before launching the existing worker in a separate process group.
Environment bytes travel only through the socket and are never stored by the handoff adapter.
SSH credentials and token variables remain outside the allowlist.
The worker retains the existing job state, workspace, bounded logs, cancellation watcher, reaper, and readiness protocol.
The node waits for that readiness acknowledgement before reporting acceptance.
Missing GUI login, absent or stopped agent, invalid request, and failed readiness are explicit refusals with no SSH fallback.

## Assurance boundary

This change introduces an operating-system and concurrent IPC boundary, with no new pure job-state transitions or resource arithmetic.
The existing source-bound job-state proofs and state codec remain unchanged.
[Kani does not model concurrency](https://model-checking.github.io/kani/rust-feature-support.html), launchd, audit-session inheritance, keychain access, or Unix socket and process-group behavior.
Those external semantics require native contracts rather than a detached proof model.

The mandatory `mise run check` runs the production handoff's socket, ownership, stale-socket, allowlist, invalid-input, size-bound, and refusal tests.
Its SSH end-to-end scenario models launchctl availability while exercising real worker startup, `on`, `run`, status, logs, and cancellation.
The two refusal regressions were checked against an implementation that accepted both unavailable cases and failed before the fix.
A credentialed native rehearsal must also install the built version, restart the GUI LaunchAgent, and submit jobs through actual SSH to check `gh auth status`, workspace selection, status, retained logs, and process-tree cancellation.
Stopping the actual agent supplies the native refusal control.
These checks establish observed adapter behavior under the native OS contract, not a theorem about all schedules or the keychain.
Revisit the alternative if a verifier supports these external semantics or a native contract exposes a mismatch.

## Consequences

macOS jobs require the user to be logged in and a compatible chat LaunchAgent to be running.
Install or update that agent with `domyjob chat service install` in the desktop login session.
The service can start workers from another build without interpreting that build's job-store format.
Linux and Windows retain their existing launch path and service behavior.
