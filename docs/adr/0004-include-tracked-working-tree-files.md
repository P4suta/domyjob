# Include tracked working-tree files in source snapshots

Status: proposed.

## Context

Remote commands need current tracked bytes even when ignore rules match.
Ignored untracked files and repository metadata must remain private.

## Decision

Merge [Git path enumeration](https://git-scm.com/docs/git-ls-files) with the existing no-follow walk.
Read working-tree bytes, omit deleted paths, and keep the selected directory as the capability boundary.
Git controls untracked ignore precedence.
Non-Git directories keep existing walking behavior.
Exclude discovered Git/common directories and metadata case variants.
Refuse unresolved repositories, links, nonregular files, path collisions, and changed read lengths.
Clear inherited Git context under [the existing decision](0001-git-process-context.md).

## Alternatives

The existing ignore walk loses ignored tracked files.
An index archive loses uncommitted content.
Disabling ignores exposes private files.

## Consequences

Git enumeration and retained directory handles add work.
Snapshots can mix concurrent file versions.
Native file system behavior remains outside scalar proofs.

## Confirmation

Require [source contracts](../verification/proofs.md), negative controls, preserved-corpus replay, and native containment fixtures.
Keep proposed until applicable checks and owner acceptance complete.
Reconsider if a native containment contract fails or a verifier can check the file system adapter.
