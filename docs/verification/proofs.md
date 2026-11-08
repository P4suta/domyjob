# Verification scope

`verification/obligations.json` is the authoritative inventory of positive harnesses, named negative controls, and native adapter boundaries.
Each harness records its production decision, input domain, unwind bound, cover count, and limitations.
`cargo xtask verify inventory` rejects missing sources, stale proof names, duplicate registrations, unregistered proofs, and missing preserved seeds.
The fast gate performs inventory inspection without starting a verifier.

## Source and tool binding

`cargo xtask verify` binds the source and physical tools before and after the attempt.
A changed input invalidates the attempt.
Native tool selection checks the supported architecture, object identity, required roles, bounded tool observations, solver configuration, and pinned versions.
A positive proof requires its exact normal success status and complete report.
A named negative requires its expected failed assertion and the expected process status.
The controller refuses unrelated exits, missing checks, unreachable required covers, incomplete reports, and solver failures.

Core harnesses import the production resource, state, source-path, cache, storage, cleanup, and diagnostic decisions.
Standalone harnesses import the actual adapters recorded in the inventory.
Finite projections and scalar proofs establish only their recorded domains.
Finite proofs don't establish native filesystem, SSH, process, launchd, Windows SDK, or external-client behavior.
The inventory links the native tests and reconsideration conditions for each boundary.

## Protocol and ingress

Quint checks the job protocol and its negative control, generates 100 fixed-seed traces, and replays each transition through the actual Rust step function.
The replay checker refuses incomplete traces, missing actions, and divergent results.
The model covers protocol ordering.
Native lock and worker ownership remain adapter contracts.

Fuzz replay copies each required checked-in seed to an isolated writable corpus.
Exploration runs every registered target for 20000 runs with seed one.
The targets exercise wire ingress, job state, source paths, process groups and inventories, cache markers, and retired-client configuration cleanup.
Cleanup fuzzing requires successful updates to be idempotent.
Retired chat requests remain rejection inputs.
Malformed or empty required seeds refuse replay before the tool starts.

Miri interprets the finite lanes in `mise.toml`, including worker handoff, state, job protocol, login replies, source paths, input ownership, capture policy, and client cleanup.
Native tests exercise real stream completion, files, permissions, locks, readiness, process-tree quiescence, result delivery, and configuration preservation.

## Execution stages

The [CI policy](../ci.md) assigns full campaigns to the release gate on the exact candidate commit.
The release matrix checks Linux, macOS, and Windows and preserves the native Windows SDK and repeated reply waits.
Owner-machine heavy tasks use the bounded container adapter.
A timeout, missing tool, rejected environment, or terminated campaign leaves verification incomplete.
No verifier-free inventory result substitutes for a completed campaign.
