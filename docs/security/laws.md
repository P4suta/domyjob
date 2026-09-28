# The laws

domyjob hands out the ability to run code on other machines.
Fixing the mistakes a review happens to find is not enough for a tool like that; the design has to make whole kinds of mistake impossible to write.
These laws are the kinds.
Each one names the property it guarantees, the mechanism that guarantees it, and the check that fails the build when the mechanism is bypassed.

These are design targets, not a claim that every mechanism and check below is implemented.
The current code has no `effects::*` abstraction, `Labeled<T, L>` covers only repository job text, and the decision-signature gate covers only `authz.rs`, `trust.rs`, `audit.rs`, `supervisor.rs`, `serve.rs`, `store.rs`, `tree.rs`, `pull.rs`, `workspace.rs`, `mcp.rs`, `node.rs`, `client.rs`, and `remote.rs`.
CI runs `cargo vet` against locked imported audits, but 267 exact-version exemptions remain.
Passing `mise run check` therefore does not establish all seven laws.

## Closure ledger

Each law closes only when its production API prevents the forbidden flow, a build gate rejects a bypass, a boundary test exercises the guarantee, and the supported operating-system matrix passes.
New findings must map to a law and a root-cause class, or add a new class with the same closure criteria.
The remaining conditions below keep a green test run from being mistaken for completion.

| Law | Current evidence | Remaining condition |
| --- | --- | --- |
| 1. No ambient authority | Some workspace and snapshot paths use confined directory handles; process arguments use typed `Invocation`, and job spawning requires a prepared environment proof | Route all filesystem, process, network, and environment effects through explicit capabilities and reject ambient calls at the build gate |
| 2. Every value carries its origin | Source-specific text types, an opaque inbound `PeerRequest`, and sealed argument traits protect some sinks | Label all peer and repository values and require a named validation or neutralization step at each sink |
| 3. Parse once, at the border | `ingress` owns several JSON and TOML decoders; the node decodes requests only as `PeerRequest` | Audit every external input and make the gate reject decoding or parsing it outside its border |
| 4. Decisions are total | Enum-match lints apply crate-wide, job phase transitions require exhaustive typed cases, production phase-tag checks use one exhaustive `Phase::kind` mapping, several lifecycle, authorization, presentation, and lock decisions use exhaustive matches, platform features use a typed capability matrix, revocation has a typed selector, remote version comparison separates invalid input, retry classification covers every remote error, and the decision-signature gate covers the modules named above | Extend decision types and the signature gate to every security decision and state transition |
| 5. Time decides nothing | Clock and timeout gates exist, production time values and the records that carry them have no comparison traits, presentation passes one observed timestamp through its duration calculations, and the one silent-peer exception is documented | Audit every remaining event decision and prove that no clock or elapsed time changes job or authorization state |
| 6. Proof obligations | CI pins and runs fuzz, Kani, ProVerif, and authorization and trust mutations | Extend reproducible mutation runs and reviewed proof results to the remaining security-critical modules |
| 7. Dependencies are audited | `cargo deny`, `cargo audit`, and a locked `cargo vet` import set check every Cargo lockfile, including fuzz; the vet gate rejects a new version without audit evidence or a new explicit exemption | Review and remove the 267 existing exact-version exemptions, especially cryptography, input parsers, and process-control crates, and require explicit review of any new exemption |

The argument builder now accepts raw assembled text only through types constructed inside `shell.rs`, `proc.rs`, and `service.rs`, and its `SafeWord` trait is sealed to the listed domain types and the internally generated `TransferId`.
Path arguments now require a sealed `SafePath` implemented only for the current executable, configured local directories and service log, a detected source root, and local distribution staging paths.
Passing an ordinary `Path` directly to `Arg::path` fails to compile.
The local directory fields are private and production construction reads the local environment; the syntax gate confines the supervisor's explicit path override to the CLI launcher.
The detected source root is private; its ordinary constructor checks a source marker, and the hook-only constructor is confined to the hook's canonicalized root.
Process working directories also require a `SafePath`; the supervisor derives its directory from a prepared root and a typed relative subdirectory, and the syntax gate rejects direct production `Command::current_dir` calls outside `Invocation`.
Job spawning now consumes a private `PreparedJobCommand` made only by `Launch::prepare`, which clears inherited and preconfigured environment entries before adding the explicit job environment and identity variables.
The syntax gate rejects a raw `Group::spawn` argument, public proof fields, proof literals outside preparation, and command extraction outside `proc.rs`; the boundary test verifies the complete resulting environment and redacted debug output.
Authorization now routes each request before the node can perform write-side maintenance.
Peer revocation classifies a private selector as a name, fingerprint, or inferred match; inferred ambiguity and unknown targets are rejected before the locked trust file is written, and explicit prefixes select one meaning.
The syntax gate requires that selector at the state-changing API and rejects public fields or construction outside its parser.
One private `RoutedRequest<E>` representation carries the principal and request through separate query and command paths; node handlers and reply functions require the corresponding typed proof.
Non-submission commands become distinct `AuthorizedConfigure`, `AuthorizedClean`, `AuthorizedRetry`, and `AuthorizedKill` proofs before their effects run, so a different command cannot supply the required argument.
An authorized submission stays in `AuthorizedSubmission` through admission and job specification creation; a different authorized command cannot be converted into that proof.
The sealed `CommandAuthority` trait and syntax gate keep maintenance and admission signatures tied to those proofs.
The ingress decoder wraps each received request in `PeerRequest`, and authorization extracts its raw request only after checking the principal's capability.
The wrapper exposes only neutralized audit context before that step; direct `Ingress` decoding into `Request` or `Submission` is forbidden by the syntax gate.
`Ingress` no longer has blanket collection or primitive implementations, so every decoder must select a named schema type.
Persisted job environment and workspace differences now pass through private transparent state types, keeping their wire format while preventing a raw map or vector from becoming a generic input capability.
The syntax gate requires each production `Ingress` implementation to name a concrete type defined in that module, including macro-generated domain newtypes, and rejects aliases and generic implementations.
CLI and MCP text enter `UserText` through source-specific types with private fields.
Repository job text remains in `RepositoryRequest` until an exact local root and machine grant produces an `ApprovedProjectJob`; its words then enter arguments through `ApprovedProjectWord` rather than `UserText`.
Repository command words, runner names, and environment values now use `Labeled<String, Repository>` at ingestion; project approval produces `Labeled<String, ApprovedRepository>`, which cannot be deserialized from input.
Required proof-field shapes are listed in one syntax-gate table, so a plain-string regression fails the build.
Approved machine values and the canonical project root remain inside the proof used to construct `Order`, so a later selector expansion or caller-supplied root cannot widen the grant.
Client origins and node-assigned job identifiers now have distinct types, and only `node.rs` defines the job ID generator.
Warm workspace paths include the submitter's owner or peer-key scope, with a test for cross-principal separation.
State-file reads now have bounded metadata, audit, and client-history budgets, and the gate confines the two larger budgets to their owning modules.
`StateFile<T>` now owns the lock for JSON read-modify-write operations; self-update holds a `StateFile<HighWater>` lock across checking and replacing the executable, and even an explicit downgrade leaves the recorded high-water mark monotonic.
Fallible state transitions use `StateFile<T>::try_update` to stop before writing when a decision rejects the change.
Production JSON state reads and replacements now pass through typed owner constructors; the syntax gate rejects raw JSON state operations and construction outside those methods, and checks their declared value types.
The remote audit witness holds its state-file lock from reading the previous head through chain verification and replacement; explicit forgetting takes the same lock, so concurrent operations have one order.
The job-phase store uses `StateFile<Phase>` for locked transitions and refuses skipped or backward lifecycle moves; tests use a separate fixture writer.
Windows private-directory creation passes a protected owner-and-SYSTEM ACL to `CreateDirectoryW` for each missing ancestor, and existing state files and directories are checked by handle for a trusted owner, known ACL entries, and the absence of reparse points.
`domyjob self secure-state --dry-run` inventories a Windows state tree before changing it; the apply command accepts only ACLs with trusted ownership and no foreign write access, tightens each candidate through the checked handle, and verifies the complete tree again.
The installed Windows state had 4,578 eligible legacy ACLs among 88,438 entries; the migration secured all candidates, a second full inventory found none, and the audit log verified all 968 existing entries.
The Mac client then rebuilt the Windows binary from the sent source automatically, `doctor win` passed, and a `hostname` job completed through the normal client path; a further scan of the now 91,250-entry state still found no legacy ACLs.
The client origin also uses the typed lock for one-time initialization, preserving its existing raw-file format while making concurrent first use choose one persisted ID.
Local configuration and project definitions have a 4 MiB parsing and file-read budget; signed release metadata also has a 4 MiB file budget, and setup reads executables only within a 64 MiB budget.
Editable TOML parsing now goes through `ingress`; the syntax gate rejects direct `DocumentMut` parsing and `serde_json::Deserializer` constructors outside that boundary.
The editable parser stays inside `ingress::EditableToml`, which exposes its parsed table for local configuration edits; `Config::layered` validates the edited result before it is written, and Clippy rejects naming `DocumentMut` in other modules.
The syntax gate also rejects imports of protected decoders, aliases of protected decoder and effect namespaces, and production glob imports, so renaming a function cannot hide a bypass.
Its owner exceptions match an exact source module or repository path, so a new file whose name merely ends in `ingress.rs` or `clock.rs` receives no exemption.
Clippy also resolves and rejects the raw `serde_json` and `toml` decoder functions, including calls hidden behind an alias or macro expansion; the `ingress` functions state their narrow exceptions.
The append-only job notes file is read through a checked handle one bounded line at a time, retaining only the last twenty notes.
The syntax gate rejects direct `read_to_end`, `read_until`, `read_line`, `fill_buf`, production `read_to_string`, and ordinary `fs::read` calls outside `bounded.rs`.
Log search keeps at most a 64 KiB prefix of each line while counting its full byte length and line number, and returns at most 1024 displayed lines.
Child command output is captured through `bounded::command_output` or `bounded::child_output` with a fixed per-purpose budget for both pipes; exceeding either budget stops and reaps the child, and the syntax gate rejects direct `.output()` and `.wait_with_output()` in production code.
The local supervisor control socket accepts at most 32 active sessions, and only a private `ControlPermit` can spawn an answer thread; dropping it releases the slot.
The MCP server accepts at most 1 MiB per input line and at most 32 active request workers; a private permit holds each slot through response writing and tracks cancellation only for active request IDs.
The syntax gate confines MCP worker spawning to a dispatch function that requires the permit and rejects unbounded line iteration in that module.
`Store::JobIds` streams published and staged job IDs, and a queued supervisor retains only the earliest live predecessor plus at most 64 tracked slot watchers and one current predecessor path.
Queue watch registration follows the exhaustive admission decision, avoiding slot watchers while an earlier queued job blocks admission and updating the watched predecessor when it changes.
Each supervisor holds a separate queue lock only while queued, and a typed admission guard serializes predecessor selection with the preparing transition and queue-lock release.
The successor's blocked watcher therefore wakes when the predecessor leaves the queue even if that job keeps running; an older or uncertain supervisor falls back to its alive lock.
A restarted supervisor first claims the alive lock, then waits for any watcher briefly holding the queue lock to release it.
Job and warm workspace lock scans both use a fixed 64-slot `OsLock::first_free` API, and an exhausted warm range fails without reusing a locked workspace.
`SlotIndex` now owns the canonical 0–63 range and lock-path construction for allocation, status, and workspace cleanup.
Queue admission and queued-job status share `Store::held_slots`, which generates exactly the canonical indices below `Concurrency::MOST`; unrelated directory entries no longer change the answer or its memory use.
Remote version comparison now returns a `VersionRelation`; an unparsable peer version stops automatic installation instead of being treated as an older release.
Client retry classification now exhaustively matches every `RemoteError`, so adding a remote error requires an explicit retry decision.
Watch retries now also classify every `ClientError`, retrying transient remote transport failures while reporting local and invalid-input failures immediately.
Job phase transitions now use an exhaustive `PhaseTransition` match; adding a `Phase` variant requires explicit decisions for every source and destination before the code compiles.
Production phase-tag checks now use `Phase::kind`, whose exhaustive match requires every new phase to receive an explicit classification; the syntax gate rejects production `matches!` checks on phase, supervisor, queue, publication, blocker, and lock-probe states.
The same gate now covers principal, request, reply, refusal code, client error, service action, location, job state, disk state, stop state, address scope, pairing state, trigger consideration, deliverable, output mode, CLI verdict, snapshot entry, and local connection reach classifications.
It rejects conditional patterns on covered enums as well as `matches!`, including `Self` patterns inside their impl blocks.
The gate discovers enum declarations across product source files, so a newly declared enum enters these checks without editing a name list.
It also rejects a catch-all binding when a match names a covered enum variant directly or inside an `Option`, `Result`, tuple, slice, or struct pattern, including variable-length patterns; forwarding the original error is narrowly exempt.
Product item macros require review, and macro definitions that emit an enum fail the gate because their generated declarations would evade the source enum inventory.
Snapshot file content is extracted through one exhaustive `Entry::file` decision before transfer, pull verification, origin lookup, and CAS reachability marking.
It also rejects aliases and variant imports that would hide these enum names from the gate, except for the reviewed local phase-transition import.
Displayed running jobs and timing samples use separate exhaustive `State` methods, and each `Family` variant must fill a typed table of five platform capabilities.
The authorization request table now distinguishes query, submission, kill, and maintained command effects; the authorized request carries that original classification through routing, and the node consumes it directly to choose maintenance and refuse a query misrouted as a command.
Submission retries now accept only an explicit missing-content refusal, with every reply and refusal-code variant classified before the client can retry.
Job listing retains at most the requested 1000 most recent jobs and refuses a list with more than 1000 unreadable records instead of returning an incomplete result.
The syntax gate confines `Store::ids` and `Store::staged_ids` collection helpers to tests and rejects direct thread spawning inside the queue loop.
Admission counts published and staged unfinished jobs under the collection lock and refuses the 65th; production staging requires a `JobAdmission` proof borrowed from that lock, while same-nonce replies remain idempotent at capacity.
Publication takes the same lock so CAS collection cannot miss a job moving between directories, and retirement preserves an in-progress staging nonce.
Source submission receives its manifest and requested blobs in one exchange while `SnapshotTransfer` holds the CAS collection lock through admission.
Submission-time retirement and low-disk collection include the incoming source manifest and its blobs in their reachability sets.
Finished-job retention keeps only the requested newest identifiers, and finished-log and stored-blob traversal use batches of 256 that close each directory iterator before deletion.
CAS collection retains at most 65,536 marked blob IDs per pass; when that budget fills, it partitions by hash prefix, rescans the roots, and removes orphaned blobs in empty partitions without retaining their IDs.
Idle workspace maintenance visits projects in bounded sorted batches and probes only the 64 canonical slots in each project; clean reports retain eight largest details plus one aggregate item, with the wire type and schema rejecting longer lists.
When an idle workspace cannot be removed after being moved to trash, low-disk reclamation continues to other workspaces and logs; an explicit clean reports the failed removal.
Trash sweeping attempts every entry before reporting its first failure, so one unremovable entry cannot prevent other entries from being removed.
`bounded::SortedScan::walk` now owns the fixed-size ordered batches and typed continue-or-stop decision used for CAS blobs, idle workspace projects, finished logs, pull journals, and legacy binary directories; each scan closes its iterator before mutating the selected entries.
Its constructor fixes the 256-key batch capacity, so callers cannot enlarge the allocation.
Pull history retains at most 31 existing entries while choosing the next journal and then removes older entries in sorted batches, preserving any journal whose lock is held.
The syntax gate rejects direct directory reads outside the reviewed streaming owner functions and refuses imports that hide the direct read.
Workspace size measurement streams directory entries through at most 64 open iterators and reports an error for deeper trees.
Short CLI fanout runs through one sixteen-worker scheduler and a sixteen-result channel, including fallible arrival processing; the syntax gate rejects new per-item CLI spawns outside the long-running paths.
Long-running watch requires a private-field `ConcurrentBatch` proof with a 64-job limit before its spawning function can run.
Incoming watch surveys accumulate only through a line buffer with a fixed 64 MiB limit, and the sender serializes through the same cap.
The line buffer distinguishes a size violation from local input or output failure in its result type.
Survey decoding refuses more than 50 jobs, matching the sender's list limit, while an incomplete final line is classified as a protocol failure.
The live display holds at most one queued survey update; when its receiver closes, the next send fails and the client ends the watch connection before waiting for its writer thread; malformed survey JSON retains its peer-origin failure classification.
The node coalesces file-change wakeups into a one-item channel, and its callback never waits for a full channel.
The supervisor coalesces queue-change wakeups into a bounded event channel; its lock-release and kill events retain room and each kill request enqueues at most once.
CAS restores and difference replies stream through a fixed buffer while checking the blob digest; a small in-memory CAS read has an explicit 64 MiB limit.
Source snapshots admit at most 100,000 entries and 16 MiB of path bytes; each disk blob can be up to 8 GiB and remains streamed, while materialized revision content has a 64 MiB total budget.
Directory hashing reads no further than each inspected file size and uses at most four workers.
The syntax gate rejects memory-mapped snapshot hashing and host CPU count as a snapshot worker limit.
Source archives stream each origin into a 64 MiB capped archive and verify its digest; job uploads also stream origins and verify their digest before accepting success.
Directory snapshots retain a confined directory handle and validated relative paths for their disk origins, so a later path replacement cannot move a read outside the selected root.
Setup output, tree reads, and log tails use explicit byte limits.
Builds and installs now receive a fresh random `TransferId`; setup fails if entropy fails.
The ID follows each operation through upload, staging, verification, and promotion, so parallel operations cannot select each other's staged binary.
SSH multiplexing has its own short `SshSessionId` and a fixed-length machine-specific `SshControlId`; the rendered control socket path reserves room for OpenSSH's temporary name and rejects unbounded expansion tokens before SSH starts.
Source builds serialize Cargo and staging in one reusable target directory with an operating-system lock; Unix hosts without `flock` or `lockf` use a target directory keyed by build stamp and source archive instead.
Before reusing a shared target, the locked build clears only domyjob's compiled artifacts, so an archive extracted before the previous build cannot make Cargo treat the old executable as current.
The source is moved to a stable compilation path only while that lock is held, and the path is removed before the lock is released.
Windows source-build scripts and archives travel as framed stdin data; a fixed bootstrap command stays below the Windows command-line limit.
Every source build verifies the staged executable's embedded build stamp before promotion.
When a remote binary cannot speak the local wire and no signed release is available, a client built from an available local checkout now archives that checkout once and installs its matching build automatically.
The build script and client share the same bounded source-stamp calculation, which parses only the `tools.rust` value from `mise.toml`; task edits no longer force a remote executable refresh, and an edited binary input is refused before a remote build can start from a stale local executable.
That source scan caps directory entries, selected files, path bytes, and nesting depth, and refuses symbolic links and path names that cannot be represented exactly as Unicode.
Managed binaries now occupy versioned regular files directly under the binary cache; a running older node's directory sweep cannot remove a newly installed copy merely because its hash sorts below older versions.
The syntax gate rejects a fixed incoming filename, and a Windows test builds two sources in one cache at once, then runs and promotes each staged executable.
Source build scripts remove their extraction directory on ordinary failure, and setup discards only its own staged files when it receives an error.
Recovery after an abrupt client or remote crash remains part of the open RC-R work.
Test fault guards now own distinct rules with path scopes; concurrent tests cannot overwrite another guard's plan, and a delayed fault counts only operations inside its declared `Path`.
The repository Nextest profile fixes four concurrent test processes on every host instead of inheriting CPU count; leaked output handles still fail the run.
The Windows crash integration harness launches children through the same explicit inherited-handle path as production, and its four test-only platform branches have an exact syntax-gate allowance.
CI installs Kani 0.68.0 and runs the existing library proof task as a required job; the current harness proves the concurrency range.
CI also checks the SHA-256 of the official ProVerif 2.05 source, builds it without the interactive simulator, and runs the three expected model proofs as a required job.
CI runs all four fuzz targets for 200,000 inputs each with a pinned nightly, cargo-fuzz version, dependency lockfile, and seed; the request target receives committed valid peer-wire seeds.
CI installs a pinned `rust-mutants` revision and requires authorization and trust mutation runs in parallel matrix jobs; each harness runs serially under a short temporary root so the unmodified baseline must pass before results count.
Workflow lint now enumerates YAML files explicitly, so the same check runs in a source snapshot that carries no Git metadata.
The focused trust run detected all 67 executed mutations locally, including error propagation after locking, random generation, persistence, and unlock, while keeping each candidate run short.
The remaining security-critical modules still need reproducible mutation gates.
The dependency tasks discover every Cargo lockfile outside generated artifact directories and run the license, source, and advisory checks for each graph; the fuzz package declares its license, and libFuzzer's NCSA license is allowed explicitly.
The advisory task uses a project-local database under ignored build output, fetches one RustSec snapshot over explicit-port HTTPS, and checks every other Cargo graph with `--no-fetch`, so the graphs use the same advisory set even when a global cache retains an SSH origin.
Crash-recovery tests wait for a supervisor's alive lock to be released before asserting that abandoned staging has been removed.
Clippy and the syntax gate now confine exclusive file creation to four ordinary-file constructors, leaving liveness and serialization to operating-system locks.

## 1. No ambient authority

**Property.** A function can touch the filesystem, start a process, open a socket, or read the environment only if it was handed the capability to do so.

**Mechanism.** Effects live in `effects::*`.
Filesystem access goes through `cap_std::fs::Dir` handles opened by the composition root, so a workspace handle cannot name a path outside the workspace, whatever the manifest says and whatever symlinks a job left behind.
Processes are started through a `Spawner` that clears the environment and applies an explicit one.
Sockets are opened through `Network`, which only the transport modules receive.

**Check.** `clippy.toml` disallows the ambient `std::fs`, `std::process::Command::new`, `std::net`, and `std::env` entry points; the `effects` modules are the only ones allowed to call them, and each does so under an `#[expect]` whose reason names the capability it implements.

## 2. Every value carries where it came from

**Property.** Data from a peer, from a repository, or from a secret cannot reach a sink that is unsafe for it.

**Mechanism.** `Labeled<T, L>` with the labels `Local`, `Peer`, `Repository`, and `Secret`.
Sinks state the labels they accept as trait bounds: an argument vector accepts only `Local`; a terminal accepts anything but renders non-`Local` text through the neutralizer; the network and the notifier never accept `Secret`.
Changing a label requires a named function that states why, such as `Peer -> Local` only through a validating parser into a domain type.

**Check.** The wire, repository, and secret types are `Labeled`; no `Deref`, `Display`, or `AsRef<str>` exists on a non-`Local` label, so the compiler refuses the unsafe flows.

## 3. Parse once, at the border

**Property.** Inside the program there are only validated domain values.

**Mechanism.** Every byte that crosses a trust boundary is decoded in one `ingress` module into exact types (`deny_unknown_fields`, bounded sizes, newtypes that reject bad values).

**Check.** The gate refuses `serde_json::from_*`, `toml::from_*`, and `str::parse` on wire or file input outside `ingress`.

## 4. Decisions are total

**Property.** Every security question has an explicit answer for every case, including cases added later.

**Mechanism.** Decisions are enums (`Decision`, `Access`, `Verdict`), never `bool`; state machines (job lifecycle, pairing, connections) are typestates or enums whose transitions are functions from one state to the next.

**Check.** `wildcard_enum_match_arm` is denied crate-wide; the gate refuses `bool`, including values wrapped in `Result` or `Option`, as a return type in the authorization, trust, audit, supervisor, pairing, and state-store modules.

## 5. Time decides nothing

**Property.** No answer the program gives depends on how fast anything ran, how long anything took, or what a clock said.
A slow machine, a suspended laptop, a clock set wrong, or a filesystem with coarse timestamps changes nothing but how long a person waits.

**Mechanism.** Every question is answered by the event it is about.
A supervisor holds its job's lock for as long as it lives, so a free lock is proof that it is gone.
Waiting for a slot, for a job, or for new log output blocks on the lock, the socket, or the condition that becomes true, never on a sleep.
A supervisor reports that it started through a pipe or a kernel event, and its death wakes the launcher just the same.
Stopping a job kills its whole process tree at once.
Files count as unchanged when their content hashes match.
Pairing offers, grants, and connections are bounded by counts and by explicit revocation, never by an expiry.
The wall clock is read in one place, `clock::Timestamp::observe`, and its value only records when something happened for a person to read.
`Timestamp` and `Elapsed` have no comparison traits in production, and that restriction also reaches job, reply, trust, and audit records that carry timestamps.
`Elapsed` exposes no numeric getter; its median and relative age labels stay inside `clock`, while JSON output retains the existing numeric millisecond field.
Job duration calculations take an explicit observed timestamp, so listing rows, dashboard activity, and history views share their caller's snapshot instead of reading the clock inside the protocol model.

**The one exception: whether a silent peer is still there.** No event can tell a peer that has stopped from one that is slow: a machine that accepts ssh but never starts the command, or a network that drops without a reset, produces no byte, no EOF, and no exit.
So time decides exactly one thing, and only in `liveness.rs`.
A node sends a heartbeat while it works on a request: a blank line before its reply, a reserved chunk length inside a stream.
The client counts every byte it sends or receives, and when nothing moves for longer than the limit, it stops its transport process; the read then ends by itself and is reported as "went silent".
The decision is only about the connection.
It never touches a job: the job keeps running, its state stays whatever its locks and records say, and the next command asks again.

**Check.** `clippy.toml` disallows `SystemTime`, `Instant`, `Duration`, `thread::sleep`, every timeout setter and timed receive, and file timestamps; the gate refuses those types and any method whose name is a sleep, a timeout, a deadline, or a file time, in tests as much as in code, everywhere but `clock.rs`.
`liveness.rs` alone may use `Instant`, `Duration`, `elapsed`, and a condition variable's `wait_timeout`; it may not read the wall clock, sleep, or set a timeout on I/O, and the gate's own tests check both halves.
The syntax gate also fixes `Timestamp` and `Elapsed` as private numeric wrappers and rejects public numeric getters, production comparison derives, and comparison implementations.
It rejects a wall-clock observation inside the protocol model, where timing functions must take an explicit presentation snapshot.

## 6. Proof obligations

**Property.** The security-critical parts do what they claim for all inputs and against an attacker who owns the network, not only for the examples in the tests.

**Mechanism.**
- ProVerif models of the paired connection prove that a recording stays secret if either X25519 or ML-KEM holds, and that a server accepts only sessions its client started (`docs/security/proverif`).
- The terminal sanitizer parses escape sequences with `vte`, the parser Alacritty uses, rather than a hand-written state machine; property tests and the `terminal` fuzz target check that no steering character comes out for any input.
- Kani proves that a concurrency is exactly one to sixty-four.
- Disk failures are injected with path-scoped test guards at file operations of the state layer, the content store, and the audit log; tests check that each is reported as an error, never taken as an absence or a success, and that the next run heals what a failed write left behind.
  Each guard owns its rules and removes only those rules on drop, so parallel tests cannot replace each other's fault plans.
- Property tests cover the sanitizer across arbitrary byte splits, both quoting rules against reference parsers, key encodings, path confinement, and ML-KEM's reaction to every flipped ciphertext bit.
- Fuzz targets cover every decoder at the border and the release verifier, run for a fixed number of inputs rather than a time.
- Fault injection makes every read and write of every handshake fail in turn and requires an error, never a panic and never a success.
- Mutation testing with njutest's `rust-mutants` shows which changes to the code no test notices; a survivor in a security module is a missing test until it is killed or proved equivalent.

**Check.** `mise run check` runs the tests; `mise run kani`, `mise run fuzz`, and `mise run proverif` run the proofs and the fuzzing; `mise run mutations:authz` and `mise run mutations:trust` measure authorization and trust mutations.

## 7. Dependencies are audited

**Property.** Code we did not write is code someone we trust reviewed.

**Mechanism.** `cargo vet` with imported audits, and an explicit allow-list for crates that handle cryptography, parsing of network input, or process control.

**Check.** CI fails on an unvetted dependency or version.

The current gate imports nine public audit sets pinned by `supply-chain/imports.lock` and checks the root and fuzz dependency graphs through the same lockfile-discovering task as `cargo deny` and `cargo audit`.
Its 267 exact-version exemptions are a review backlog, not audits; `cargo vet` reports 257 exemptions in the root graph and 253 in fuzz, with overlap between them.
Consequently, the present gate prevents an unnoticed version change but does not yet satisfy this law's review property.
