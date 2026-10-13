# Include tracked working-tree files in source snapshots

Status: Proposed.

## Context

A remote command needs the current project files even when an ignore rule matches a file already tracked by Git.
Filesystem ignore walking alone omits those files, while an index archive would lose uncommitted content.
Untracked ignored files and repository metadata remain private.
Snapshot paths must remain portable, bounded, and confined to the selected working directory.

## Decision

Combine the existing ignore walk with the current directory's [NUL-terminated tracked paths](https://git-scm.com/docs/git-ls-files).
Read the working-tree files, omit deleted tracked paths, and exclude repository metadata from both sources.
Keep non-Git directories on the existing ignore walk.
Remove inherited repository-local Git context before querying the selected directory, following [the Git context decision](0001-git-process-context.md).

The [pure parser](../../crates/domyjob-core/src/source_paths.rs) bounds path bytes, record length, and file count before constructing validated portable paths.
The archive rejects symlink and reparse-point components and reads files through the existing `cap_std` directory capability, which confines reads even if an ancestor changes after inspection.
Archive size and case-insensitive path collision checks remain authoritative.

## Assurance boundary

Source-bound Kani harnesses quantify the parser budget transition over every admitted integer counter and byte value and check the production metadata classifier.
A separate false metadata assertion must fail for the expected assertion.
The fuzz corpus checks malformed records, metadata exclusion, and canonical round trips through the production parser.
Native Git fixtures check repository and global ignores, modified and deleted tracked files, subdirectory paths, and replaced directory links.

Git index contents, filesystem identity, concurrent mutation, and operating-system capability semantics remain external assumptions covered by native contracts rather than the arithmetic proof.
The snapshot does not promise one atomic view of files changing concurrently.
Revisit that boundary if a native containment contract fails or a suitable verifier can check the filesystem adapter.

## Consequences

Remote verification receives current tracked files without sending ignored untracked files.
Git is consulted when available, and Git enumeration failures after repository discovery are reported instead of dropping tracked paths.
The source archive now uses the existing capability dependency during build-time archive generation as well.
Index snapshots and disabling ignore rules were rejected because they respectively lose working-tree changes and include private untracked files.
