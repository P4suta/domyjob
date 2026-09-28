# domyjob v1 architecture

The first release is an SSH job runner.
An SSH login is the authority to run a command on a host; domyjob does not add a second peer identity, pairing protocol, listener, or network discovery service.
The client sends a bounded snapshot of the selected directory and an exact command vector to a node started through SSH.
The node accepts the job, assigns its identifier, stores its input, and launches a detached supervisor.
The client can then list jobs, inspect one status, read its log, wait for its outcome, and stop it.

## Boundaries

The command line parses local choices into validated machine names, paths, and argument vectors.
The SSH transport carries length-framed messages with a fixed maximum size and an explicit protocol version.
The node decodes one frame into validated domain types before it can inspect a request.
Snapshot entries use validated relative paths and a confined directory handle; extraction never follows a symlink from the archive or the destination.
Only the node creates job identifiers and job records.
The node and supervisor share one durable state transition API, and each transition is checked while the job record is locked.
The supervisor's operating-system lock is evidence of liveness; clocks are only for display.
The process launcher receives a validated argument vector and an explicit environment, and passes no client or SSH secrets to a job.
Remote text reaches a terminal only through a neutralizer.

## Scope and deployment

The first release supports `run`, `on`, `ls`, `status`, `logs`, `wait`, `kill`, `machines`, `setup`, and `doctor` on macOS, Linux, and Windows.
The source directory includes uncommitted files and honors ignore rules.
The first connection installs a node from a signed binary or the local checkout, and a wire mismatch refreshes it automatically before the request is sent.
An interrupted install never replaces the last working node.
There is one protocol and one node implementation across the three operating systems.

Configuration templates, hooks, file pullback, pairing, a network listener, notifications, live dashboards, and MCP are outside the first release.
They can be designed against the same job and transport core after that core is complete.

## Mechanical closure

The `domyjob-core` crate is `no_std` and contains external-byte ingress, validated values, and decisions.
The application crate keeps filesystem, process, SSH, and environment authority in `effects` and composes it in `app`.
Core modules cannot access the filesystem, process, network, environment, or clock APIs.
The build rejects direct decoding outside `ingress`, ambient effects outside `effects`, unbounded allocation from input, wildcard matches on domain enums, and raw strings at command, path, state, and terminal sinks.
Each rule has a failing fixture, and the full check runs on all three operating systems.
