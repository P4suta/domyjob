# Fast validation and release verification

Pushes and pull requests run the fast gate.
Releases require the full verification inventory on the exact candidate commit.
The [decision record](adr/run-heavy-verification-at-the-release-gate.md) defines this boundary.

## Fast gate

`mise run check:scope` is the pre-push entry point, backed by `cargo xtask ci hook`.
`mise run check` runs the complete fast gate on the current host without scope selection.
The gate checks formatting, native and cross-target Clippy, architecture and structure, Windows compiler contracts, fuzz-target types, spelling, workflow syntax, duplication, and ordinary tests.
Dependency changes also run deny, audit, and vet in CI; `mise run check` includes those checks locally.
`cargo xtask verify inventory` checks every registered source, proof name, negative control, and Parent refinement without starting a verifier.

Documentation changes run spelling checks.
Workflow and task changes run the shared static checks, cross-target Clippy, and the task package's native tests.
Product code, dependencies, toolchains, build configuration, and unknown paths select the complete fast gate.
No scope starts Kani, Quint, Miri, fuzz exploration or replay, mutation testing, stress repeats, or another host.
`check:fleet` and its owner-machine adapters remain optional commands outside hooks and required CI.

[CI](../.github/workflows/ci.yml) runs on pull requests and merge groups, on the pinned `ubuntu-26.04` runner.
It provisions no verifier and has no macOS, Windows, fuzz, or stress job.
The required summary keeps the name `ci-success` and depends only on `changes`, `lint`, `deny`, `vet`, and `test`.
`cargo xtask ci gate` rejects failed or cancelled jobs and accepts skipped jobs only when the validated plan did not select them.
Actions CodeQL, dependency review, and Scorecard keep their existing separate summaries.

The planner compares exact Git objects using a NUL-delimited diff with rename detection disabled.
The pre-push hook unions every outgoing ref range and includes staged, unstaged, and untracked changes.
A new branch uses the merge base with local `main`, or the validated remote's tracking `main` when passed to `cargo xtask ci hook REMOTE`.
Missing history, merges, forks, and malformed inputs select the complete fast gate.
They never select a heavy campaign.

## Release gate

[Release check](../.github/workflows/release-check.yml) accepts `candidate_ref` through `workflow_dispatch` or `workflow_call` and publishes nothing.
It resolves that ref once, and every job checks out the resulting immutable commit.
It runs the full Kani inventory and named negative controls, Quint induction and fixed-seed trace replay, preserved fuzz corpora, and every Miri lane.
Every fuzz target receives 20000 exploration runs with seed one in an isolated writable corpus.
The native matrix runs Clippy, architecture checks, inventory consistency, and the full test suite on Linux, macOS, and Windows.
Linux also checks native resource ownership; Windows checks actual SDK contracts and repeats the three reply-wait scenarios ten times.
Fixed counts define exploration; a runner timeout leaves evidence incomplete.

[The tag-triggered release workflow](../.github/workflows/release.yml) calls this gate with `github.sha`.
Its version, policy, and dependency checks wait for the entire gate, so signing and distribution cannot start after a failed or incomplete verification.
Release-plz keeps opening version PRs and creating tags; ordinary release PR CI still runs only the fast gate.
Manual release checks do not grant permission to sign or publish.

## On-demand verification

On owner machines, set `DOMYJOB_VERIFICATION_IMAGE` to a prepared Linux amd64 Docker image before running `mise run verify`, `miri`, `fuzz`, or either mutation task.
The image needs the pinned mise tools, Kani 0.68.0, cargo-fuzz 0.13.2, nightly-2026-10-01 with Miri and rust-src, and the selected mutation engine when needed.
Direct `cargo xtask verify` selectors use the same adapter; inventory inspection needs no container.
Native task implementations refuse execution outside Actions or the verification container.
GitHub Actions executes the checks natively on its hosted runners.

The adapter resolves the image to its immutable Docker identity and refuses a missing image.
Source is read-only, while evidence and fuzz artifacts use separate writable mounts below `target/verification-container`.
Each command receives two CPUs, 2 GiB without swap, 256 processes, and a 900-second diagnostic limit.
A bounded cleanup restores cache ownership.
Limit termination is incomplete evidence and cannot pass a release check.
Set `DOMYJOB_VERIFICATION_MACHINE` as well to run the bounded adapter through a verified domyjob source snapshot on a configured Linux owner machine.
No hook, pull request, or hosted release requires that machine or its image cache.

## Build isolation

The CI adapters disable mise's automatic tool installation.
Workflows provision their selected tools explicitly.
Cargo and mise children use validated directories under `target/ci-build` or `target/ci-build-alt`, separate from the running task executable.
Directory links that escape the checkout and invalid directory layouts fail before a child starts.
Windows receives ordinary drive or UNC paths after canonical identity checks.
Dependency caches remain separate by OS, architecture, toolchain, and lockfile.
