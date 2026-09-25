# The laws

domyjob hands out the ability to run code on other machines.
Fixing the mistakes a review happens to find is not enough for a tool like that; the design has to make whole kinds of mistake impossible to write.
These laws are the kinds.
Each one names the property it guarantees, the mechanism that guarantees it, and the check that fails the build when the mechanism is bypassed.

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

**Check.** `wildcard_enum_match_arm` is denied crate-wide; the gate refuses `-> bool` in the authorization, trust, and pairing modules.

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
- Disk failures are injected with the `fail` crate at every file operation of the state layer, the content store, and the audit log; tests check that each is reported as an error, never taken as an absence or a success, and that the next run heals what a failed write left behind.
  A fault names the path it hits, so it never reaches another test running at the same time.
- Property tests cover the sanitizer across arbitrary byte splits, both quoting rules against reference parsers, key encodings, path confinement, and ML-KEM's reaction to every flipped ciphertext bit.
- Fuzz targets cover every decoder at the border and the release verifier, run for a fixed number of inputs rather than a time.
- Fault injection makes every read and write of every handshake fail in turn and requires an error, never a panic and never a success.
- Mutation testing with njutest's `rust-mutants` shows which changes to the code no test notices; a survivor in a security module is a missing test until it is killed or proved equivalent.

**Check.** `mise run check` runs the tests; `mise run kani`, `mise run fuzz`, and `mise run proverif` run the proofs and the fuzzing; `rust-mutants run --file` measures one module.

## 7. Dependencies are audited

**Property.** Code we did not write is code someone we trust reviewed.

**Mechanism.** `cargo vet` with imported audits, and an explicit allow-list for crates that handle cryptography, parsing of network input, or process control.

**Check.** CI fails on an unvetted dependency or version.
