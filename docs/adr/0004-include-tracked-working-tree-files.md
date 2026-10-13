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
When Git confirms a repository, load ancestor ignore rules so a selected subdirectory excludes parent-ignored untracked files while retaining ignored tracked working bytes.
Canonicalize the selected directory once, discover the canonical repository root through bounded Git output, and retain the selected directory as the archive boundary.
Ask [Git to decode its excludes-file path](https://git-scm.com/docs/git-config) before supplying global ignore rules, including quoted paths and repository configuration precedence.
Resolve a relative excludes-file path at the repository root and anchor global patterns there, following [Git's exclude pattern rules](https://git-scm.com/docs/gitignore#_pattern_format).
Remove inherited repository-local Git context before querying the selected directory, following [the Git context decision](0001-git-process-context.md).

The [pure parser](../../crates/domyjob-core/src/source_paths.rs) bounds path bytes, record length, and file count before constructing validated portable paths.
Exclude ASCII case variants of repository metadata conservatively on every platform.
The shared archive and fingerprint reader captures every parent directory without following links and holds those handles through the final regular-file read.
Unix uses the public `cap-primitives::fs::open_dir_nofollow` API and safe `rustix::fs::openat` with `NOFOLLOW`, `NONBLOCK`, and `CLOEXEC`.
Windows uses the documented [NtCreateFile relative RootDirectory](https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntcreatefile) and `FILE_OPEN_REPARSE_POINT` for each component, then requires an ordinary disk file before reading.
Read limits admit at most 64 MiB, observe at most one extra byte, and reject growth or truncation relative to the opened file's admitted length.
Archive size and case-insensitive path collision checks remain authoritative.

## Assurance boundary

Source-bound Kani harnesses quantify the parser budget transition over every admitted integer counter and byte value and check the production metadata classifier.
A separate false metadata assertion must fail for the expected assertion.
An additional source-bound scalar proof and named negative control check actual read admission, length+1 limits, and exact completion.
The fuzz corpus checks malformed records, metadata exclusion, and canonical round trips through the production parser.
Native Git and file fixtures check ignored working bytes, ancestor rules, competing relative excludes files and repository-root anchored patterns against actual Git, quoted global configuration, real metadata case aliases, parent/final link replacements, writerless FIFO refusal, retained-handle renames, and changed read lengths.

Git index contents, canonical path discovery, ignore matching, filesystem identity, concurrent mutation, and operating-system capability semantics remain external assumptions covered by native contracts rather than the arithmetic proof.
The snapshot does not promise one atomic view of files changing concurrently.
Revisit that boundary if a native containment contract fails or a suitable verifier can check the filesystem adapter.

## Consequences

Remote verification receives current tracked files without sending ignored untracked files.
Git is consulted when available, and Git enumeration failures after repository discovery are reported instead of dropping tracked paths.
The source archive now uses the existing capability dependency during build-time archive generation as well.
Index snapshots and disabling ignore rules were rejected because they respectively lose working-tree changes and include private untracked files.
