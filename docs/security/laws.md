# The laws

domyjob hands out the ability to run code on other machines.
Fixing the mistakes a review happens to find is not enough for a tool like that; the design has to make whole kinds of mistake impossible to write.
These laws are the kinds.
Each one names the property it guarantees, the mechanism that guarantees it, and the check that fails the build when the mechanism is bypassed.

These are design targets, not a claim that every mechanism and check below is implemented.
The current code has no `effects::*` or `Labeled<T, L>` abstraction, the decision-signature gate covers only `authz.rs`, `trust.rs`, `audit.rs`, `supervisor.rs`, `serve.rs`, `store.rs`, `tree.rs`, `pull.rs`, `workspace.rs`, `mcp.rs`, and `node.rs`, and CI does not run `cargo vet`.
Passing `mise run check` therefore does not establish all seven laws.

## Closure ledger

Each law closes only when its production API prevents the forbidden flow, a build gate rejects a bypass, a boundary test exercises the guarantee, and the supported operating-system matrix passes.
New findings must map to a law and a root-cause class, or add a new class with the same closure criteria.
The remaining conditions below keep a green test run from being mistaken for completion.

| Law | Current evidence | Remaining condition |
| --- | --- | --- |
| 1. No ambient authority | Some workspace and snapshot paths use confined directory handles; process arguments use typed `Invocation` | Route all filesystem, process, network, and environment effects through explicit capabilities and reject ambient calls at the build gate |
| 2. Every value carries its origin | Source-specific text types and a sealed argument-word trait protect some sinks | Label all peer and repository values and require a named validation or neutralization step at each sink |
| 3. Parse once, at the border | `ingress` owns several JSON and TOML decoders and domain newtypes validate selected fields | Audit every external input and make the gate reject decoding or parsing it outside its border |
| 4. Decisions are total | Enum-match lints apply crate-wide and the decision-signature gate covers the modules named above | Extend decision types and the signature gate to every security decision and state transition |
| 5. Time decides nothing | Clock and timeout gates exist, with one documented silent-peer exception | Audit every remaining event decision and prove that no clock or elapsed time changes job or authorization state |
| 6. Proof obligations | Test, fuzz, Kani, ProVerif, and mutation tasks are defined | Make required proof runs and their reviewed results reproducible in CI for the security-critical modules |
| 7. Dependencies are audited | `cargo deny` and `cargo audit` run as checks | Add a maintained `cargo vet` audit set and make an unvetted dependency fail CI |

The argument builder now accepts raw assembled text only through types constructed inside `shell.rs`, `proc.rs`, and `service.rs`, and its `SafeWord` trait is sealed to the listed domain types and the internally generated `TransferId`.
CLI and MCP text enter `UserText` through source-specific types with private fields.
Repository job text remains in `RepositoryRequest` until an exact local root and machine grant produces an `ApprovedProjectJob`; its words then enter arguments through `ApprovedProjectWord` rather than `UserText`.
Approved machine values and the canonical project root remain inside the proof used to construct `Order`, so a later selector expansion or caller-supplied root cannot widen the grant.
Client origins and node-assigned job identifiers now have distinct types, and only `node.rs` defines the job ID generator.
Warm workspace paths include the submitter's owner or peer-key scope, with a test for cross-principal separation.
State-file reads now have bounded metadata, audit, and client-history budgets, and the gate confines the two larger budgets to their owning modules.
`StateFile<T>` now owns the lock for JSON read-modify-write operations; self-update holds a `StateFile<HighWater>` lock across checking and replacing the executable, and even an explicit downgrade leaves the recorded high-water mark monotonic.
The client origin also uses the typed lock for one-time initialization, preserving its existing raw-file format while making concurrent first use choose one persisted ID.
Local configuration and project definitions have a 4 MiB parsing and file-read budget; signed release metadata also has a 4 MiB file budget, and setup reads executables only within a 64 MiB budget.
The append-only job notes file is read through a checked handle one bounded line at a time, retaining only the last twenty notes.
The syntax gate rejects direct `read_to_end`, `read_until`, `read_line`, `fill_buf`, production `read_to_string`, and ordinary `fs::read` calls outside `bounded.rs`.
Log search keeps at most a 64 KiB prefix of each line while counting its full byte length and line number, and returns at most 1024 displayed lines.
Child command output is captured through `bounded::command_output` or `bounded::child_output` with a fixed per-purpose budget for both pipes; exceeding either budget stops and reaps the child, and the syntax gate rejects direct `.output()` and `.wait_with_output()` in production code.
The local supervisor control socket accepts at most 32 active sessions, and only a private `ControlPermit` can spawn an answer thread; dropping it releases the slot.
The MCP server accepts at most 1 MiB per input line and at most 32 active request workers; a private permit holds each slot through response writing and tracks cancellation only for active request IDs.
The syntax gate confines MCP worker spawning to a dispatch function that requires the permit and rejects unbounded line iteration in that module.
`Store::JobIds` streams published and staged job IDs, and a queued supervisor retains only the earliest live predecessor plus at most 64 slot watchers and one predecessor watcher.
Job listing retains at most the requested 1000 most recent jobs and refuses a list with more than 1000 unreadable records instead of returning an incomplete result.
The syntax gate confines `Store::ids` and `Store::staged_ids` collection helpers to tests and rejects direct thread spawning inside the queue loop.
CAS restores and difference replies stream through a fixed buffer while checking the blob digest; a small in-memory CAS read has an explicit 64 MiB limit.
Source snapshots admit at most 100,000 entries and 16 MiB of path bytes; each disk blob can be up to 8 GiB and remains streamed, while materialized revision content has a 64 MiB total budget.
Directory hashing reads no further than each inspected file size and uses at most four workers.
The syntax gate rejects memory-mapped snapshot hashing and host CPU count as a snapshot worker limit.
Source archives stream each origin into a 64 MiB capped archive and verify its digest; job uploads also stream origins and verify their digest before accepting success.
Directory snapshots retain a confined directory handle and validated relative paths for their disk origins, so a later path replacement cannot move a read outside the selected root.
Setup output, tree reads, and log tails use explicit byte limits.
Builds and installs now receive a fresh random `TransferId`; setup fails if entropy fails.
The ID follows each operation through upload, staging, verification, and promotion, so parallel operations cannot select each other's staged binary.
SSH multiplexing has its own shorter `SshSessionId`, and the rendered control socket path is checked before SSH starts.
Source builds use the archive's `BlobId` under the client build stamp for their Cargo target directory, so different sources cannot share an executable cache entry.
The syntax gate rejects a fixed incoming filename, and a Windows test builds two sources in one cache at once, then runs and promotes each staged executable.
Source build scripts remove their extraction directory on ordinary failure, and setup discards only its own staged files when it receives an error.
Recovery after an abrupt client or remote crash remains part of the open RC-R work.
Test fault guards now own distinct rules with path scopes; concurrent tests cannot overwrite another guard's plan, and a delayed fault counts only operations inside its declared `Path`.
The repository Nextest profile fixes four concurrent test processes on every host instead of inheriting CPU count; leaked output handles still fail the run.
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
The wall clock is read in one place, `clock::Timestamp::observe`, and its value only records when something happened for a person to read: `Timestamp` has no ordering, so it cannot be compared to decide anything.

**The one exception: whether a silent peer is still there.** No event can tell a peer that has stopped from one that is slow: a machine that accepts ssh but never starts the command, or a network that drops without a reset, produces no byte, no EOF, and no exit.
So time decides exactly one thing, and only in `liveness.rs`.
A node sends a heartbeat while it works on a request: a blank line before its reply, a reserved chunk length inside a stream.
The client counts every byte it sends or receives, and when nothing moves for longer than the limit, it stops its transport process; the read then ends by itself and is reported as "went silent".
The decision is only about the connection.
It never touches a job: the job keeps running, its state stays whatever its locks and records say, and the next command asks again.

**Check.** `clippy.toml` disallows `SystemTime`, `Instant`, `Duration`, `thread::sleep`, every timeout setter and timed receive, and file timestamps; the gate refuses those types and any method whose name is a sleep, a timeout, a deadline, or a file time, in tests as much as in code, everywhere but `clock.rs`.
`liveness.rs` alone may use `Instant`, `Duration`, `elapsed`, and a condition variable's `wait_timeout`; it may not read the wall clock, sleep, or set a timeout on I/O, and the gate's own tests check both halves.

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

**Check.** `mise run check` runs the tests; `mise run kani`, `mise run fuzz`, and `mise run proverif` run the proofs and the fuzzing; `rust-mutants run --file` measures one module.

## 7. Dependencies are audited

**Property.** Code we did not write is code someone we trust reviewed.

**Mechanism.** `cargo vet` with imported audits, and an explicit allow-list for crates that handle cryptography, parsing of network input, or process control.

**Check.** CI fails on an unvetted dependency or version.
