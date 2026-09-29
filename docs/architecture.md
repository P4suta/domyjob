# Architecture

domyjob is an SSH job runner and AI agent chat with one protocol, one persistent state model, and one implementation for macOS, Linux, and Windows.
The first release does not read or migrate state written by the former implementation.

## Pure decisions

`domyjob-core` is `no_std` and owns bounded identifiers, portable paths, validated commands, framed wire messages, ingress decoding, job state transitions, and the chat ledger's admission and exchange rules.
It also renders service definitions and client registrations as pure text.
Its request and reply enums require exhaustive handling when the protocol changes.
Its job state has accepted, starting, running, and finished phases, with a terminal outcome that cannot transition again.
Malformed stored states cannot deserialize into a running job with a zero process ID.

The binary composes the pure decisions with SSH, filesystem, watch events, locks, and processes.
Each effect has one owner module, and [the engineering rules](engineering.md) list how the compiler, lints, and gate enforce that and the other invariants.

## Submission and persistence

The client creates a random submission ID before connecting and prints it.
The node derives the job ID from that submission ID, so retrying the same request is idempotent.
A repeated submission with different content is rejected.
The node publishes a complete staged job record with one directory rename.
A worker lock is the evidence of liveness; a dead worker produces the explicit `lost` outcome.
Cancellation is persisted and delivered through a file change event before the process tree is terminated.
Finished jobs may be removed; active jobs may not.
Each admission removes the oldest finished jobs beyond the newest 32, so logs and workspaces never accumulate without bound.
A job leaves the store with one rename into its trash, and its tree is then removed as far as it can be;
a tree that cannot be removed yet, such as one holding files a container wrote as root, waits there and never stops an admission.

Every path under the state root is defined in `layout`.
The job runner keeps its store in a directory named by its format, so builds of different formats never read each other's jobs;
a node removes another format's store once no process of that format runs and none has opened it for a week,
and a stored format is the digest of a checked-in specimen rather than a version number.
State records have a 1 MiB size bound and are written through a private, synchronized replacement path.
The storage adapter checks ownership and Unix mode or Windows ACL before reading existing state.
The command runs in a Unix process group or Windows Job Object, with a guard against a stranded Unix process group.
Its standard output and standard error share one pipe, and the worker stores at most the first 256 MiB and counts the rest.
When the process tree exits, the worker stores what is already in the pipe and finishes, even if an escaped descendant still holds the pipe open.
The node clears the SSH session environment before launching a job and adds a small explicit set of operating-system and toolchain variables.

## Transport and source

Each SSH request contains one length prefixed JSON control frame with a 1 MiB limit.
The frame carries no version, because a client only talks to a node of its own build.
Calls carry deadlines, keep the connection alive with SSH keepalives, and share one connection per machine on Unix.
A `run` request may carry a 64 MiB tar snapshot of the current directory, including uncommitted files.
The source scanner rejects links, nonregular files, nonportable names, and case collisions.
The node checks the declared size and digest before extracting through a confined directory capability.
Remote text is neutralized before terminal output.

The build script fingerprints every source file under `crates/` and `xtask/`, the root Cargo manifests and lockfile, and the pinned tool configuration.
The client compares that fingerprint at startup and rebuilds from the checkout if necessary, unless `DOMYJOB_REFRESH=never` is set.
Local rebuilds honor `CARGO_TARGET_DIR` and use a target slot that no running client occupies on Windows.
Nodes use build-specific executable paths under `~/.cargo/domyjob/versions/`, so installing a new build does not replace the executable of a running worker.
The remote command runs that path through the account's shell and exits with status 97 when it is missing.
The client then installs the matching binary from a portable archive embedded in the client build and repeats the request, which every request tolerates.
That archive remains available when the original checkout is unavailable.
Every process started from an installed build holds a shared lock inside its build directory, and each node request removes the other builds whose lock it can take.
A persistent Cargo target directory on each remote host reuses compiled dependencies.

Automatic local rebuild requires a development checkout, while remote installation always uses the embedded source archive.
Remote builds require `mise` and Rust on the remote host.
The build fingerprint is a deployment identity, not an authentication credential.
OpenSSH authenticates the host and user; domyjob does not expose a separate listener or pairing protocol.

## Verification

`mise run lint` checks formatting, Clippy for the host, Linux, and Windows, architecture gates, spelling, workflow syntax, and duplicate code.
`mise run test` tests the complete workspace, including the stored format specimens, a search of every reachable state of a chat ask across three machines,
and an end-to-end suite that simulates several machines on one host with fake SSH and AI clients.
`mise run fuzz` fuzzes wire ingress, job state transitions, and chat ledger convergence.
`mise run check:fleet` sends this checkout to Linux and Windows and runs Clippy, gates, and tests there.
The CI matrix runs Clippy and tests on macOS, Linux, and Windows.
The `commit-msg` hook refuses a fix to product code that stages no test.
