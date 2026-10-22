---
date: 2026-10-22
---

# Frame login-session requests

## Status

Proposed.

## Context

The [login-session handoff](0003-run-macos-jobs-in-login-session.md) must acknowledge worker readiness independently of host scheduling.
Its unframed request uses socket EOF as the message boundary.
A native repeated e2e run captured both peers blocked in receive: the node waiting for its reply and the agent waiting for request EOF.
That observation does not establish why the native EOF was absent.
Removing the socket deadline exposes this protocol dependency instead of establishing progress.

## Decision

Use the existing bounded control-frame encoding for the request.
The receiver reads the validated length and exactly that payload before decoding, validating, and launching the worker.
A complete frame needs no subsequent EOF observation.
Keep the ordered reply write and half-close contract.
Neither direction has a socket deadline.
Use a versioned socket directory so an incompatible running agent is refused through the existing missing-agent diagnostic.
Compatible builds continue sharing one agent for the same user home and protocol version.

The shared e2e peer adapter waits for a response or owned-process exit and captures output independently of inherited handles.
Periodic process, request, and output-tail diagnostics accompany the outer nextest hang guard.

Longer deadlines retain machine-speed-dependent failures.
Retrying a handoff can duplicate worker launch and cannot establish the request boundary.
Waiting indefinitely for EOF retains the observed circular wait.

## Consequences

The existing frame codec supplies the payload bound and length validation.
The native handoff tests require complete frames to finish even when another read would refuse.
Truncated headers or payloads fail before launch.
The agent must be updated with `domyjob chat service install` in the desktop login session when moving from the prior unframed protocol.
This change preserves the job store, environment allowlist, worker ownership, and ordered reply adapter.
