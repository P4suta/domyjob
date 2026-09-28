# Architecture

domyjob is an SSH job runner with one protocol, one persistent job model, and one implementation for macOS, Linux, and Windows.
The first release does not read or migrate state written by the former implementation.

## Pure decisions

`domyjob-core` is `no_std` and owns bounded identifiers, portable paths, validated commands, framed wire messages, ingress decoding, and job state transitions.
Its request and reply enums require exhaustive handling when the protocol changes.
Its job state has accepted, starting, running, and finished phases, with a terminal outcome that cannot transition again.
Malformed stored states cannot deserialize into a running job with a zero process ID.

The `xtask gates` check rejects `std` imports in core and OS branches outside the platform and process adapters.
Workspace Clippy settings deny wildcard matches, unchecked unwraps, silent error conversion, and direct calls to selected effect APIs.
The binary composes the pure decisions with SSH, filesystem, watch events, locks, and processes.

## Submission and persistence

The client creates a random submission ID before connecting and prints it.
The node derives the job ID from that submission ID, so retrying the same request is idempotent.
A repeated submission with different content is rejected.
The node publishes a complete staged job record with one directory rename.
A worker lock is the evidence of liveness; a dead worker produces the explicit `lost` outcome.
Cancellation is persisted and delivered through a file change event before the process tree is terminated.
Finished jobs may be removed; active jobs may not.

State records have a 1 MiB size bound and are written through a private, synchronized replacement path.
The storage adapter checks ownership and Unix mode or Windows ACL before reading existing state.
The command runs in a Unix process group or Windows Job Object, with a guard against a stranded Unix process group.
The node clears the SSH session environment before launching a job and adds a small explicit set of operating-system and toolchain variables.

## Transport and source

Each SSH request contains one length prefixed JSON control frame with a 1 MiB limit.
A `run` request may carry a 64 MiB tar snapshot of the current directory, including uncommitted files.
The source scanner rejects links, nonregular files, nonportable names, and case collisions.
The node checks the declared size and digest before extracting through a confined directory capability.
Remote text is neutralized before terminal output.

The build script fingerprints every source file under `crates/` and `xtask/`, the root Cargo manifests and lockfile, and the pinned tool configuration.
The client compares that fingerprint at startup and rebuilds from the checkout if necessary.
Local rebuilds honor `CARGO_TARGET_DIR` and use another target slot on Windows when the active executable occupies Cargo's output path.
Before a remote RPC, it compares the node fingerprint and installs a matching binary from a portable archive embedded in the client build.
That archive remains available when the original checkout is unavailable.
Nodes use build-specific executable paths under `~/.cargo/domyjob/versions/`, so installing a new build does not replace the executable of a running worker.
A persistent Cargo target directory on each remote host reuses compiled dependencies.

Automatic local rebuild requires a development checkout, while remote installation always uses the embedded source archive.
Remote builds require `mise` and Rust on the remote host.
The build fingerprint is a deployment identity, not an authentication credential.
OpenSSH authenticates the host and user; domyjob does not expose a separate listener or pairing protocol.

## Verification

`mise run lint` checks formatting, Clippy, architecture gates, spelling, workflow syntax, and duplicate code.
`mise run test` tests the complete workspace.
`mise run check:fleet` sends this checkout to Linux and Windows and runs Clippy, gates, and tests there.
The CI matrix runs Clippy and tests on macOS, Linux, and Windows.
