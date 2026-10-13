# Protect bootstrap publication through first use

Status: Proposed.

## Context

A complete newly installed node can be removed before its first request acquires the installed-build usage lock.
Separate archive upload and first-request connections leave both a lifetime gap and an opportunity to lose the original request.
Concurrent publishers need to admit the existing winner without overwriting it or rejecting a valid first request.
Windows directory publication cannot retain an open child usage-file handle through rename.
Installer diagnostics and cleanup must not consume request bytes or replace the primary failure.

## Decision

Send the bounded source archive and unchanged original request through one [ordered writer](../../crates/domyjob/src/transport/input.rs).
Consume exactly the archive prefix and keep the original request and payload on inherited stdin for the first node.
Build in a private ticket directory with isolated installer input and diagnostic stderr.
The [publication adapter](../../crates/domyjob/src/builds.rs) holds the existing selection lock while publishing a private candidate and acquiring its final shared usage lock.
An existing complete winner is validated and held before the losing candidate is discarded.
The [first-node caller](../../crates/domyjob/src/transport.rs) retains that admission through the actual child outcome.
Its touched reaper CLI shares the existing typed positive process-group constructor, source-bound proof, and replayed parser rather than retaining an unowned inline parser.
After successful rename, cleanup is disarmed for the reused staging name and removes only the empty owned outer directory.
Preserve the current two-idle-build retention policy; later storage-lifecycle work can change that policy after this lifetime bridge is present.

## Assurance boundary

The source-bound Kani proof imports the actual segmented writer and establishes the finite six-byte ordering, partial-refusal, and flush contract with five reachable covers.
The actual process-group constructor separately proves positive-only admission over every `i32`, with three covers and an exact named false ownership control.
Its named false success control must produce the expected counterexample.
Native fixtures exercise real selection and usage locks, a controlled pruner, directory publication and ACLs, existing-winner refusal, retained first-node admission, and stage-reuse failures.
Source-bound script fixtures check original binary frame, snapshot, and EOF delivery, isolated installer input/output, truncation, and primary-preserving cleanup faults.
Windows fixtures use the actual native prefix reader and inherited overlapped pipe before a real Rust child consumes the exact remainder.
Operating-system locks, filesystem identity, PowerShell inheritance, CLR marshalling, and Win32 completion remain native boundary obligations rather than consequences of the bounded writer proof.
Revisit these boundaries if a checked native contract fails or the publication, first-request, or pipe protocol changes.
Historical pruners that do not obey the selection-lock protocol are outside the new adapter's concurrent-lock guarantee.

## Consequences

A first request retains its published node without changing the pinned build or ordinary two-idle pruning policy.
Competing installers reuse a checked winner while preserving their original request.
Private staging consumes temporary space until its owned cleanup finishes.
Keeping a child usage handle open through publication was rejected because the supported Windows rename boundary refuses it.
Relying on a fixed idle count was rejected as a lifetime guarantee because the candidate can become an older idle build before its first use.
Reopening the original request in another SSH connection was rejected because it preserves the publication gap and complicates exact input ownership.
