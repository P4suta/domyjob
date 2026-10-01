# Effect contracts and finite verification

The purpose of an operating-system regression is to protect a contract shared by its callers.
An individual mutation is evidence to investigate, rather than a requirement to introduce another helper and test for every changed expression.

## Causes and enforced boundaries

| Failure class | Why the earlier design allowed it | Required boundary |
| --- | --- | --- |
| Windows replacement failed while a reader held the destination | The caller selected a persistence API whose Windows replacement behavior differed from standard rename; three callers manually assembled staging and cleanup | `StagedFile` owns synchronization, writer closure, rename, and post-success cleanup suppression |
| A released build lock still appeared held | A plain `File` hid the lock lifetime and treated closing one descriptor as sufficient; a duplicate can retain the same lock | `OsLock` owns acquisition and explicit release, has no clone or raw-file escape, and unlocks in Drop |
| An SDK operation received a valid handle with insufficient rights | One raw handle owner represented events, processes, threads, snapshots, and jobs without encoding access rights | Kind and access capabilities with checked private constructors |
| An output constructor bypassed the gate through an alias or macro | The original gate recognized one path spelling | Resolved method bans plus ownership checks for reserved constructor names and macro tokens |
| An approved effect exception hid another forbidden operation | A broad Clippy expectation reserved effects but did not identify their allowed owners | An explicit owner registry and an independent check for forbidden replacement and cross-owner lock operations |
| A capability check missed an alternate spelling or a generated exception | Aliases, custom trait implementations, and nested conditional attributes could hide the operation from a local syntax check | Capability owners have no type or import aliases or globs, only an explicit Drop implementation, and recursively checked conditional exceptions |
| A compiler proof appeared complete with repeated evidence | Counting fixtures or primary spans did not establish distinct required categories and failure sites | An exact contract inventory, actual leaf paths, one primary per diagnostic, and independent ACL and SID lifetime sites |
| A mutation appeared to require another runtime fixture despite being forbidden in CI | The ordinary source mutation build ran tests without the mandatory Clippy pass | Separate static rejection from observed runtime behavior in the evidence and closure assessment |
| An external submission outlived its runner | Signing, upload, waiting, and publication shared one process lifetime without a durable continuation | Preserve the original signed bytes, source-bound submission IDs, and attested handoff; query each existing ID once in a later bounded job |
| An external submission succeeded before its receipt could be saved | Output storage was checked after the external side effect, and cleanup could discard the returned ID | `ReceiptDestination` checks storage before signing; receipt preservation precedes cleanup, whose failure still rejects success |
| A status response could be confused with publication authority | Progress, cryptographic verification, source identity, and publication were represented as procedural steps | `Origin`, `VerifiedHandoff`, and `AcceptedToken` are separate private capabilities; publication requires all three and rechecks unchanged bytes and protected source |
| Archive generation and validation disagreed on a host | The native tar format was implicit, while isolated fixtures never exercised the actual producer and consumer together | Emit the portable USTAR format and pass every target's actual native archive through the strict inventory and extraction contract |

The duplicate-descriptor contract demonstrates why close alone is insufficient.
An inherited descriptor from another concurrently spawned child is a plausible explanation for the observed restored-control lock failure, but the responsible child was not captured.
The implementation removes dependence on that explanation by explicitly unlocking while a real duplicate remains open in the contract test.

The staging type proves that the writer was synchronized and closed before its replacement method is available.
It preserves temporary-file cleanup when synchronization or replacement fails.
State ownership and permissions remain checked by `state_io`, and linked configuration files retain the user-file adapter's target and mode policy.
Executable installation uses the same synchronization path.
Standard rename can still fail because of permissions, sharing modes, filesystem behavior, or other operating-system errors, and those failures remain explicit results.

The lock guard proves acquisition succeeded before a caller receives the guard.
Consuming release reports an unlock failure.
Drop attempts unlock and reports an error without panicking; closing the file remains the final fallback.
The type cannot make the kernel infallible or force a caller to keep a guard for the entire intended critical section.
The actual retention and exclusion behavior is therefore also covered by caller contracts.

Windows workflows receive resource-specific owned or borrowed capabilities rather than a generic HANDLE owner.
An event or process can expose a narrower wait view without duplicating its ownership.
Job transitions retain the actual child borrow until configuration, assignment, watch setup, and resumption have succeeded.
ACL, SID, and ACE views borrow their security allocation and cannot survive its owner.
Suspended process creation remains part of the existing launch factory's reviewed behavior; the typed job states do not prove that an arbitrary std::Child was created suspended.

## Shared verification categories

| Boundary | Contract categories |
| --- | --- |
| Staged replacement | Complete bytes with a live reader; reader snapshot retained; failed replacement cleans up the temporary file and preserves the destination; existing private-state and linked-configuration policies |
| Lock guard | Shared and exclusive acquisition; held versus free versus absent; Drop and consuming release with a live duplicate; installed-build retention and exclusion |
| Windows SDK leaf | BOOL and immediate last-error capture; NULL and invalid-handle rejection; direct error status; typed wait outcomes; exit-code output; descriptor-borrowed ACL and SID lifetime; job failure cleanup and successful launch |
| Architecture gates | Forbidden replacement through an approved exception; unregistered raw owner; cross-owner lock operation; output aliases and macro tokens; SDK imports outside the exact leaves |
| Independent mutation evidence | Actual source splice; restored control; fresh artifact provenance; raw execution attribution; missing or conflicting evidence remains unaudited |
| Release continuation | Exact original repository, run, attempt, main ancestry, source and artifact; original attestation; duplicate-field and bounded archive rejection; pending, rejected and accepted native outcomes; no automatic resubmission |
| Release resource and publication ownership | Storage preflight before side effects; receipt retained on cleanup failure; owned credential and extraction cleanup; private capabilities without fabrication or cloning; manual origins never publish; exact draft assets before immutable publication |

Compiler checks cover prohibited capability combinations and ownership lifetimes without executing invalid SDK operations.
An operating-system capability proves the requested resource kind and granted access at construction, rather than promising that later operations cannot fail.
The small FFI constructors and their ABI assumptions remain a reviewed boundary backed by native contract tests.

## Completion boundary

1. Implement the common ownership boundaries and the mandatory static gates.
2. Pass the finite shared contracts, negative compiler checks with passing controls, project lint, and the existing macOS, Linux, and Windows suites.
3. Freeze the relevant source, configuration, engine, and test-target inventory before the final affected-source measurement.
4. Give each selected mutation one bounded round with its restored control, preserve the raw evidence, and audit it independently.
5. Classify remaining observations as a public behavior defect, an existing static rejection, a reviewed structural or caller-reachability reason, or unresolved evidence.

A public behavior defect or a new SDK result convention can justify another common contract.
A survivor count alone does not justify restarting individual fixture expansion.
Timeouts, failed controls, and errors are never counted as detections, and historical runs are not promoted to evidence for a later source tree.
Native observations remain separate from sealed core and repository-gate scores.

Release changes use the same finite boundary: freeze their source, pass the shared contracts and mandatory gates on each affected host, then run one nonpublishing rehearsal of the final source.
An external pending result leaves final notarization acceptance incomplete; it does not justify rebuilding or adding more individual fixtures.
Source capabilities constrain the order of trusted operations, while native checks and reviewed dependency versions cover behavior that Rust's type system cannot prove.
