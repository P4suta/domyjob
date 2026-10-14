# Include tracked working-tree files in source snapshots

Status: Proposed.

## Context

A remote command needs the current project files even when an ignore rule matches a file already tracked by Git.
Filesystem ignore walking alone omits those files, while an index archive would lose uncommitted content.
Untracked ignored files and repository metadata remain private.
Snapshot paths must remain portable, bounded, and confined to the selected working directory.

## Decision

Combine the current directory's [NUL-terminated tracked and untracked paths](https://git-scm.com/docs/git-ls-files) with the existing no-follow walk.
Read the working-tree files, omit deleted tracked paths, and exclude repository metadata from both sources.
Keep non-Git directories on the existing ignore walk.
In Git mode, `ls-files --others --exclude-standard` owns ancestor, `.gitignore`, `info/exclude`, and global-ignore precedence while tracked paths retain their current working bytes.
The walker still applies `.ignore` rules and entry-shape policy, but cannot reinstate a Git-ignored untracked file.
Canonicalize the selected directory once and capture the actual Git directory, common directory, and working-tree root through individually bounded [absolute Git queries](https://git-scm.com/docs/git-rev-parse).
Exclude both captured metadata subtrees, including custom names, from the merged paths and bind case aliases through native canonicalization before archive admission.
Refuse metadata or bare roots and unresolved declared repositories rather than treating them as ordinary non-Git input.
Retain the selected directory as the archive capability boundary.
Non-Git global rules keep the existing [Git-decoded excludes-file path](https://git-scm.com/docs/git-config) and selected-root pattern base.
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
Four actual-implementation Kani harnesses check the typed component-prefix matcher for all three-letter lowercase metadata names and the same four exact root, descendant, common, and component-prefix sibling cases.
One unchanged sequence and cover per harness preserves the complete combined domain and four covers within the unchanged per-harness timeout.
The native path adapter supplies actual `Path::components()` to that same matcher, with native and Miri controls for rooted and relative namespaces.
Its named false control must reject delivery of a custom metadata descendant.
The fuzz corpus checks malformed records, metadata exclusion, and canonical round trips through the production parser.
Native Git and file fixtures check ignored working bytes, ancestor rules, competing relative excludes files and repository-root anchored patterns against actual Git, quoted global configuration, real metadata case aliases, parent/final link replacements, writerless FIFO refusal, retained-handle renames, and changed read lengths.
Additional real Git fixtures check separate and distinct common metadata, forced tracked metadata, relative pointers with spaces, metadata-root refusal, malformed ancestor declarations, bare repositories, symlink and case aliases, and `info/exclude` precedence against `.gitignore` negation.

Git index contents, native path canonicalization and case folding, declaration stability during collection, ignore matching, filesystem identity, concurrent mutation, and operating-system capability semantics remain external assumptions covered by native contracts rather than the bounded path or arithmetic proofs.
The snapshot does not promise one atomic view of files changing concurrently.
Without Git or an explicit `.git` declaration, discovery cannot identify an arbitrary custom bare directory, and ordinary `HEAD` or `objects` working names do not become repository heuristics.
Revisit that boundary if a native containment contract fails or a suitable verifier can check the filesystem adapter.

## Consequences

Remote verification receives current tracked files without sending ignored untracked files.
Git is consulted when available, and Git enumeration failures after repository discovery are reported instead of dropping tracked paths.
The source archive now uses the existing capability dependency during build-time archive generation as well.
Index snapshots and disabling ignore rules were rejected because they respectively lose working-tree changes and include private untracked files.
