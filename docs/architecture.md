# Architecture

## Core and adapters

`domyjob` runs persistent jobs over SSH on macOS, Linux, and Windows.
The runner keeps unknown stored formats and refuses to interpret them.
The current and previous runner specimens define the supported compatibility boundary.

`domyjob-core` owns identifiers, commands, framed messages, ingress decoding, job states, resource policies, cache identities, and storage decisions.
The core is `no_std` and forbids unsafe code.
The binary connects those decisions to files, SSH, locks, watches, and processes through the [effect owners](engineering.md).
Exhaustive request, reply, and state enums require explicit handling of every operation.

## Job ownership and delivery

The client creates a submission identity before connecting.
The node derives the job identity from that submission.
Repeating the same request returns its existing execution, while conflicting content causes refusal.
The node publishes a staged job with one directory rename.

A worker lock establishes supervisor liveness.
A missing supervisor produces the explicit `lost` outcome.
The worker persists cancellation and stops its owned Unix process group or Windows Job Object.
The runner removes the workspace only after confirming native quiescence and recording the result.

The client acknowledges a terminal retrieval after printing all requested output.
`run --wait` and `on --wait` print the outcome and log together.
`status`, `wait`, and `logs` acknowledge their requested output.
`--keep` preserves the result and log.
A failed connection or output leaves the result available for retry.
Private completion receipts preserve status queries and submission deduplication after consumption.
The runner refuses retrieval of consumed log bytes.

Source jobs share a Cargo target cache by project and archived build inputs.
Each active user holds a lease.
A changed input key replaces obsolete idle slots after their users finish.
Cache roots carry `CACHEDIR.TAG`.
The [storage decision](adr/0005-retire-storage-after-confirmed-use.md) defines retirement conditions.

## Private storage

`layout` defines every state path.
A specimen digest names each runner store.
The compatibility adapter preserves the previous runner's lock paths and readable terminal tombstones.
A build keeps another format's store while active users, jobs, or completion receipts still reference it.

State and control records have a 1 MiB bound.
The storage adapter checks Unix ownership and mode or Windows ACLs before reading existing state.
A `StagedFile` synchronizes and closes its writer before replacement.
An owning `OsLock` explicitly unlocks even when another descriptor remains open.

Retirement moves a job into private trash with one rename.
Cleanup uses retained directory capabilities and refuses foreign ownership.
The cleanup adapter removes links without following them and leaves regular-file permissions unchanged.
An entry that cleanup can't remove waits for a later attempt without blocking new jobs.

The worker stores at most the first 256 MiB of combined standard output and standard error and counts omitted bytes.
After process-tree completion, the worker finishes even when an escaped descendant still holds an output handle.
Jobs receive a cleared environment with an explicit operating-system and toolchain allowlist.

## macOS login service

The node sends the matching worker executable, job identity, state root, and allowed environment through a private framed Unix socket.
The login service launches the worker inside the desktop session and answers its readiness signal.
Credentials stay in the keychain or 1Password.
Missing login or service availability causes refusal without a direct SSH fallback.
Linux and Windows launch workers directly.

`domyjob service install` updates the historical `dev.domyjob.chat` identity to run `service serve`.
The migration removes recognized chat registrations and preserves unrelated configuration and chat history.
The [retirement decision](adr/retire-agent-chat-and-retain-job-service.md) separates job runtime ownership from agent coordination.

## SSH and source snapshots

Each SSH request carries one length-prefixed JSON control frame with a 1 MiB limit.
A client contacts only the node of its own build.
Calls use SSH keepalives and share a connection per machine on Unix.

A `run` request can include a tar snapshot of the selected directory, bounded to 64 MiB.
Tracked files supply current working-tree bytes, including ignored tracked changes.
Git-ignore rules exclude untracked files, while the walker preserves `.ignore` rules.
The scanner excludes deleted files, repository metadata, links, nonregular files, nonportable paths, and case collisions.

Bounded Git queries identify canonical repository and common directories before discovery.
Subdirectory snapshots honor ancestor ignore rules without widening the archive root.
Git resolves excludes-file configuration before the walker applies it.
Canonical metadata identity and ASCII casing checks protect both tracked and untracked selection.

Archive and fingerprint readers hold the root and every captured parent directory.
They open the final file relative to its captured parent without following links.
Growth or truncation beyond the admitted length causes refusal.
The node checks the declared size and digest before extraction through a confined capability.
The output adapter neutralizes remote text before terminal display.

## Build lifetime and verification

The build script fingerprints product and task source, Cargo manifests, the lockfile, and pinned tool configuration.
Development clients can rebuild from the checkout when the fingerprint changes.
`DOMYJOB_REFRESH=never` disables that refresh.
Remote installation uses the embedded source archive even when the original checkout disappears.

Nodes run fingerprint-specific executables under `~/.cargo/domyjob/versions/`.
Installed processes hold shared build locks.
Bootstrap holds usage from candidate publication through the first node request.
Pruning preserves active and pinned builds and removes superseded idle installations.
Old running binaries keep their historical pruning behavior until replacement.

The [validation policy](ci.md) defines the required fast gate and release campaigns.
The release matrix checks native behavior on macOS, Linux, and Windows.
The [verification inventory](verification/proofs.md) separates finite decisions from native adapter assumptions.
