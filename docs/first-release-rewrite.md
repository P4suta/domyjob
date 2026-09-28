# First-release rewrite

The first release has no compatibility requirement with the previous CLI, wire format, or stored jobs.
The replacement is developed in `domyjob-core` and `domyjob-next` while the existing `domyjob` library supplies the remaining state, lock, and process adapters.

## Enforced boundaries

`domyjob-core` is always `no_std` and cannot import `std`.
It owns validated identifiers, portable paths, commands, wire messages, ingress checks, and the total job state transition function.
The `xtask gates` task checks that boundary and limits calls from `domyjob-next` into the old crate to the named effect adapters.
Wire requests and replies use exhaustive enums, one framed JSON control message, explicit size limits, and a build fingerprint computed from every file under `crates/` and `xtask/`.
The same source scanner runs at build time and client startup, so adding, changing, or deleting a source file invalidates the local and remote build automatically.

## Job model

The client assigns a random submission ID before connecting and prints it so an uncertain submission can be retried.
The node derives the job ID from that typed submission ID, making repeat submissions idempotent without a second persistent index.
An accepted job is published by one directory rename after its request, state, and optional workspace are complete.
The worker owns an operating-system liveness lock, and a missing worker becomes an explicit `lost` outcome.
Cancellation is a durable request delivered through a file-change event and terminates the isolated process tree.
Completed jobs can be removed individually or in a batch, while active jobs are protected from deletion.

## Source and deployment

`run` archives the current directory, including uncommitted work, and verifies the archive size, digest, portable paths, and file types before extraction through a confined directory capability.
The source archive limit is 64 MiB, and the control-frame limit is 1 MiB.
On client startup, source files are compared with the compiled fingerprint and the local client is rebuilt when they differ.
Before an RPC, the client checks the remote fingerprint and transfers a validated checkout archive over SSH to rebuild and install a matching remote binary when needed.
The remote installer keeps a persistent Cargo target directory so subsequent updates reuse compiled dependencies.
Automatic rebuild requires the development checkout and `mise` on the remote host.

## Cutover conditions

The default binary is still the previous `domyjob` CLI, and `domyjob-next` is the replacement under test.
Cutover requires extraction of the needed operating-system adapters from the old application crate and a final review of the new command surface and storage limits.
The old CLI and its duplicated application logic can then be removed without weakening the process, filesystem, or transport boundaries.
