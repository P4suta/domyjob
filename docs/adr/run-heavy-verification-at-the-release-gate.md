---
date: 2026-10-22
---
# Run heavy verification at the release gate

## Status

Accepted.
This applies the [shared verification decision](https://github.com/P4suta/dotfiles/blob/main/docs/adr/run-heavy-verification-at-the-release-gate.md) to domyjob.

## Context

Pre-push validation ran the full proof inventory, Miri, tests, dependency checks, and the owner's native Linux and Windows fleet.
Pull requests also ran proofs, model checks, fuzzing, and tests on three operating systems.
Daily development waited for release-sized campaigns and depended on private hosts.
The existing tag release path checked dependencies before signing but did not require the complete candidate verification inventory.

## Decision

The [fast gate](../ci.md#fast-gate) runs on the current host before push and on one pinned Linux runner for pull requests and merge groups.
It checks formatting, Clippy including cross-target checks, architecture, inventory consistency, and ordinary tests.
`ci-success` keeps its required name and depends only on fast jobs.
Changes still include their harnesses, models, and fuzz targets.

The [release check](../../.github/workflows/release-check.yml) resolves one candidate SHA and verifies every proof, model, preserved corpus, Miri lane, and supported native OS.
Fuzz exploration and stress repeats use fixed counts and seeds.
The [release path](../../.github/workflows/release.yml) requires this gate before version checks, signing, or distribution.
The workflow can also verify a candidate on demand without publishing.

Owner-machine heavy commands require the bounded container adapter brought from commit fc23750.
It uses immutable image identity, read-only source, separate output mounts, and fixed memory, CPU, process, and diagnostic limits.
Hosted Actions keeps native execution.
Optional owner hosts supply no required release evidence.

## Alternatives

Selecting proofs or platforms by the changed contract still makes routine changes wait for campaigns.
Keeping those campaigns on PRs alone moves that wait to review and merge.
Removing the campaigns would leave releases without the required evidence.
Requiring the owner's fleet would make public verification depend on private infrastructure.

## Consequences

Pushes and PRs finish after deterministic local checks and ordinary tests.
A proof, fuzz, or platform failure can reach main and must be repaired before the next release can proceed.
Releases take longer because they run the complete inventory on the exact candidate.
An on-demand run does not gate a commit, push, PR, or merge.
A timeout, missing tool, or failed check leaves verification incomplete.
