# Engineering rules

## Effects and types

Keep decisions in `domyjob-core` and effects in the binary's adapters.
The core is `no_std` and forbids unsafe code.
Use validated identifiers, exhaustive enums, owned resources, and checked transitions.
Don't replace errors with absence or give reads and queues unbounded capacity.

Reserved effects belong to registered private `raw` modules.
Their functions forward directly to the reserved API, while the adapter supplies its policy.
`cargo xtask gates` rejects unregistered owners, malformed exceptions, logic inside raw forwarding functions, and comments outside the permitted generated or tool-required forms.
Clippy resolves the forbidden APIs and types independently of the syntax gate.
Native foreign-function leaves and the end-to-end harness have exact scoped exceptions.

`Output`, created by `main`, owns standard output.
`process::command` creates children with closed standard streams.
`platform::clock` owns time and timed waits.
`domyjob-core::ingress` owns decoding.
`state_io` owns private state, `layout` defines state paths, and `platform::user_files` changes user configuration while preserving existing links and permissions.

A `StagedFile` synchronizes and closes its writer before replacing its destination.
An `OsLock` explicitly releases its lock even when a duplicate descriptor survives.
Directory publication uses checked private-directory capabilities.
Cleanup holds directory handles and refuses foreign ownership.
It doesn't follow links or change regular-file permissions.
Failed cleanup leaves retired entries available for a later attempt without blocking admission.

Required Linux resource policies grant an owned permit before launch and verify effective limits for the whole workload.
Windows process creation, configuration, assignment, watch setup, and resumption use typed capabilities.
The native compiler contracts reject wrong kinds, access rights, ownership, lifetimes, and transition order using the pinned SDK types.
A stop treats an already absent process as success.

## Platforms and formats

Operating-system capabilities belong behind the `System` traits in `platform` and `process`.
Each native implementation supplies every required capability.
Host and cross-target Clippy check all supported branches.

A specimen digest identifies each stored format under `crates/domyjob/formats/`.
The specimen tests refuse an unrecorded encoding change.
Use `DOMYJOB_UPDATE_FORMATS=1 cargo test` only when intentionally updating the stored contract.
Preserve the prior runner specimen and its supported read paths.
The runner keeps data in unknown historical formats and refuses to read it.
The wire protocol connects a client only to the node of the same build.

Storage retirement requires confirmed execution, delivery acknowledgment, and released references.
Active installations, cache leases, and unretrieved results survive later work.
The [storage decision](adr/0005-retire-storage-after-confirmed-use.md) records the lifecycle boundaries.

## Verification

`mise run check` runs the complete fast gate on the current host.
`mise run lint` checks formatting, host and cross-target Clippy, architecture, Windows compiler contracts, fuzz-target types, spelling, workflows, and duplication.
`mise run test` runs workspace tests with nextest, including stored formats and the native job and bootstrap scenarios.
Dependency checks run deny, audit, and vet.
A product fix requires a regression test in the staged change.

`cargo xtask verify inventory` checks source and proof registration without starting a verifier.
The [verification guide](verification/proofs.md) describes positive harnesses, named negatives, covers, model traces, and preserved fuzz corpora.
Native operating-system effects remain explicit tested boundaries.
A clean inventory doesn't establish an unrun proof or campaign.

The [CI policy](ci.md) assigns Kani, Quint, Miri, fuzz exploration, mutation work, stress repeats, and the native OS matrix to their required stages.
Owner-machine heavy commands require a bounded verification container.
Optional fleet checks don't become prerequisites for the fast gate.
Mutation expectations name reviewed equivalences in the current source.
Preserve nonzero and inconclusive outcomes, inspect native leads, and confirm restored controls before claiming detection.
