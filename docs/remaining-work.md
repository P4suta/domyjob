# Remaining work at the AI chat integration checkpoint

Work stopped at the user's request on 2026-09-28.
This is a resumable checkpoint, not a claim that the chat integration or the broader design goal is complete.
Do not resume implementation, deployment, or provider calls without a new user request.

## Context and constraints

- Working branch: `rewrite-v1`.
- Previous completed rewrite checkpoint: `f7044d8` (`docs: distinguish bounded log replies from stored output`).
- Reference chat implementation: `feature/chat` at `e418029`, following `97fc4cc`.
- The reference worktree is `/Users/yasunobu/projects/github.com/P4suta/domyjob-chat`; it was clean when inspected and was not modified.
- The product has not had its first release.
  The user explicitly permits incompatible redesign and replacing old implementation rather than preserving legacy behavior or state migrations.
- The user's broader objective is a small, deterministic design with shared abstractions and mechanical checks that accelerate development as well as execution.
  Do not equate adding individual validations with completing that objective.
- Commits are authorized; pushes are not.
- Use pinned tools through `mise` and set `RUSTC_WRAPPER=` in this environment.
- Remote builds and commands must go through this checkout's domyjob.
  Read the `domyjob` and `multi-machine` skills before remote work.
  They were found at `~/.claude/skills/domyjob/SKILL.md` and `~/.claude/skills/multi-machine/SKILL.md`.
  There is also a tracked manual at `skills/domyjob/SKILL.md`.

## Implemented in this checkpoint

| Area | Implementation |
| --- | --- |
| Chat domain | `crates/domyjob-core/src/chat.rs`: effect-free event validation, message modes, audiences, ordering and response relationships. |
| Chat wire | `crates/domyjob-core/src/chat_wire.rs`: bounded batches, validated origins, sequence and clock checks, strict decoding through existing ingress. |
| Persistence | `crates/domyjob/src/chat.rs`: private redb file, transactions, idempotent merges, origin identity, acknowledgments, resolutions, turn claims and notification markers. |
| SSH integration | `chat_sync.rs`, `transport.rs`, `app.rs`: peer discovery and pinning, bounded bidirectional exchange, existing automatic deployment. |
| CLI and operations | `chat_cli.rs`: one operation interpreter shared by CLI and MCP, owner and agent addressing, rooms, send/ask/reply, inbox/thread/watch/open, setup/doctor. |
| MCP | `mcp.rs`: local stdio JSON-RPC, initialization, tool catalog, shared input schemas and dispatch, bounded input and output. |
| AI execution | `chat_runner.rs`, `process/chat.rs`: Claude Code, Codex and OpenCode adapters, structured successful completion, session retention, failure and interruption handling. |
| Process lifecycle | Existing job isolation, reaper and readiness mechanisms now also support chat workers; per-agent launch and execution locks bound worker creation. |
| Terminal output | `domain::terminal_text` is shared by job logs and chat rendering. |
| Documentation | README chat section and `docs/chat-architecture.md`. |

The old service, pairing, listener, configuration system and job implementation were not merged back into the rewrite.
Chat uses the existing SSH transport and account trust boundary.
Each host creates its own durable chat origin and keeps its AI session data local.

The reference implementation had a concrete validation gap: received replies bypassed the original-request validation and unique-resolution marker.
Local append and incoming merge now validate responder, conversation, audience and original request transactionally.
An incoming reply whose request has not arrived rolls back its batch without advancing the acknowledgment.

Another discovered decoder issue was that an internally tagged unit enum variant could accept unknown fields despite `deny_unknown_fields`.
Chat `Identity {}` and `Omitted {}` use empty struct variants to reject those fields without changing their JSON representation.
There are regression tests for this behavior.

## Verification evidence

These results describe this checkpoint, not a release approval.

| Check | Result |
| --- | --- |
| Final `env RUSTC_WRAPPER= mise run lint` | Passed after the final enum and exhaustive-match corrections. |
| Final `env RUSTC_WRAPPER= mise run test` | Passed: 36 binary tests, 16 core tests, 4 xtask tests; 56 total. |
| Architecture gates | Passed as part of lint. |
| Exact duplication gate | Passed: 14 clones, 96 duplicated lines, 0.97%, threshold 1%. |
| `mise run deny` | Passed for root and fuzz dependency graphs; existing allowance/duplicate-version warnings remain. |
| `mise run audit` | Passed for both graphs against the fetched RustSec database. |
| `mise run vet` | Passed: root 11 fully audited / 107 exempted; fuzz 2 fully audited / 25 exempted. |
| Fuzz graph compilation | Passed after updating `fuzz/Cargo.lock` for the new core BLAKE3 dependency. |
| Updated chat fuzz campaigns | Not run; no chat-specific seeds added. |
| Linux and Windows for these changes | Not run. |
| Real multi-machine chat exchange | Not run. |
| Fake-provider process/MCP end-to-end scenario | Not completed; first registration attempted a source refresh during a concurrent edit and compilation stopped before registration or provider invocation. |
| Real Claude/Codex/OpenCode turns | Not run; no paid model calls were made. |

The previous rewrite had passed Mac, Linux and Windows checks and real job/deployment scenarios before chat was added.
Those results do not cover the new chat code or the changes to shared process startup.

The new redb 4.3.0 dependency is pinned and explicitly exempted in `supply-chain/config.toml`.
It has not been independently audited here.
The vet success must not be described as all dependencies having been audited.

## Remaining work, in resumption order

### 1. Complete integration verification

- Repair and run the preserved [fake-provider E2E draft](verification/chat-e2e-draft.py) in an isolated `DOMYJOB_STATE` and temporary working directory.
  It is an unverified draft, not a passing or CI-registered test.
  It contains machine-specific paths, formats event sequence IDs as decimal rather than 16-digit hexadecimal, and assumes outdated `EventData` JSON paths such as `data.details.text`.
  Fix these assertions against the current serialized types before relying on its result.
  It currently targets Unix using Python `fcntl`; Windows needs an equivalent native fixture.
- Verify first managed ask, subsequent session resume, exactly one invocation per question and exactly one durable answer.
- Verify incomplete or malformed output, nonzero provider exits, missing executables, changed session IDs and excessive output become failures without returning a partial answer.
- Verify an attached local agent can receive the owner's ask through MCP and reply to its exact ID without starting a provider.
- Verify simultaneous asks serialize per agent while different agents can progress independently.
  Exercise the last-scan/worker-exit race and assert repeated sync does not accumulate waiting workers.
- Verify worker termination after claim records `interrupted` on recovery and never silently reruns the external turn.
  Include process-tree cleanup, startup failure and disconnect scenarios.
- Exercise all advertised MCP tools through actual stdio, not only direct operation tests.
  Check initialization, IDs, malformed input, errors, output limits and clean stdout while source refresh or remote deployment occurs.
- Run the standard Linux and Windows checks through domyjob after local fixtures pass.
  Verify shared job startup/readiness still works after its argument-vector and stdin generalization.
- Run an actual two/three-machine exchange: agent discovery, private DM, room message, offline delivery, reconnect, repeated delivery, response, failure and session metadata redaction.
  Use isolated state where practical and do not leave test agents or jobs running.
- Validate the installed provider entrypoints on each supported OS, especially Windows executable or script shims, stdin prompts, resume syntax, credentials and permissions.
  Structured-output fixtures and documentation inspection are not substitutes for this compatibility check.

### 2. Finish synchronization correctness and bounds

- Add direct tests of the exchange interpreter in `chat_sync.rs`.
  The last change bounds a reply acknowledgment to the submitted batch's final sequence, or its `after` cursor for an empty batch, and requires the client to receive that exact acknowledgment.
  Core batch tests do not exercise this complete interpreter.
- Test duplicate/replayed exchange, stale cursors, acknowledgments past the sent batch, empty batches, concurrent synchronization in both directions, origin changes and failures between receive/ack steps.
- Test dependency ordering across peers when a responder is visited before the question's origin.
  Current sync continues after a peer failure and a subsequent invocation can succeed after the parent arrives.
  It does not retry deferred peers within the same invocation.
  Decide whether to add a bounded retry of only dependency failures and distinguish them from permanent failures.
  Do not blindly retry SSH authentication failures.
- Review the blanket mapping of chat server errors to `InvalidRequest` in `transport.rs`.
  Missing causal dependencies, resource limits, invalid requests and storage/worker failures should have useful typed outcomes if the client needs to react differently.
- Measure and, if necessary, remove redundant deployment handshakes during each peer exchange.
  `transport::chat` currently performs the existing deployment check for each request.
- Decide the intended background delivery behavior explicitly.
  Current commands synchronize on entry; writes synchronize after commit; ask/watch/open continue polling while active.
  There is no background service performing reconnects while all clients are closed.
- Review cancellation/deadlines for stalled SSH calls.
  `ask --timeout` is checked between exchanges; a blocked SSH request can extend the wall-clock wait beyond that timeout.
  Keep time limits out of deterministic job/turn state transitions.

### 3. Finish persistence and resource policy

- Document and test quota exhaustion and recovery.
  The store currently allows at most 10,000 event rows and 128 MiB of serialized event payload.
  This is not a physical redb file-size limit; pages and metadata can use additional space.
  There is no implemented archive/compaction/pruning workflow.
- Decide how terminal resolutions remain writable when event storage is full.
  A question accepted immediately before exhaustion must not become indefinitely unresolved because its answer or failure cannot be persisted.
- Add persistence fault/crash tests at turn claim, session update, response commit, notification marker and acknowledgment boundaries.
  Transaction unit tests alone do not simulate process termination or disk failure.
- Review notification failure semantics.
  Notification markers are persisted before OS notification; a failed notification is logged and is not retried for that message.
  Message content remains available in the inbox.
- Review repeated full-history projections.
  Point event lookup is indexed, but agents/rooms/inbox/thread/dispatch still scan and sort a bounded ledger, sometimes repeatedly per operation.
  Prefer shared projections or indexes and explicit pagination over growing special-case scans.
- Bound construction of large CLI/MCP results earlier.
  MCP rejects an encoded reply above 1 MiB after constructing it; large history projections may already consume much more memory.

### 4. Complete mechanical enforcement and maintainability

- Add chat wire seeds to `fuzz/seeds/wire`, then run the existing fuzz campaigns.
  Include identity, exchange, replies/failures, omitted events, malformed modes, unknown fields and cursor boundaries.
  Consider a stateful chat merge/response target for ordering and duplicate-delivery laws.
- Replace remaining representable invalid combinations where it materially simplifies the design.
  Agent registration still uses `managed: bool`, `session: Option<String>` and a string directory, including a redacted remote placeholder.
  Separate local managed, local attached and synchronized public metadata if doing so removes repeated checks.
  Many event/address fields also remain strings validated at boundaries rather than distinct newtypes.
- Review the split between pure validation, stored projections and effect adapters as a whole.
  Do not claim the repository-wide ideal is complete merely because the current test and lint suite passes.
- Keep new checks enforcing properties across all paths, especially local append versus remote merge and CLI versus MCP.
  Avoid tests that only repeat constructor implementation.
- Resolve meaningful duplication instead of relaxing the 1% gate.
  The current 0.97% result leaves little room; repeated wire-reply match arms and test setup are possible consolidation points.
- Review MCP responsiveness during a long `chat_ask`.
  The current stdio interpreter executes calls sequentially and does not cancel an in-progress operation on cancellation notifications.
- Review provider parser schema drift and session-state publication order.
  The adapters require successful structured completion, but foreign protocol decoding still needs ongoing contract tests.

### 5. Finish product documentation and release checks

- Update `docs/chat-architecture.md` with quota behavior, causal-dependency retries, crash semantics, notification limits and final verification evidence.
- Confirm the deliberate changes from the reference feature are acceptable for the first-release workflow.
  `chat setup` adds/pins SSH peers and prints MCP configuration; it does not automatically edit Claude/Codex/OpenCode configuration.
  `chat doctor` checks stored configuration and synchronization but does not comprehensively test provider installation or authentication.
- Document peer removal, identity replacement, agent/session replacement, history cleanup and the supported operational recovery path, or implement the required commands.
  These management flows are not complete.
- Update the tracked skill/manual to include final chat commands and the local MCP workflow.
- Recheck installed/packaged deployment with chat dependencies and source fingerprints after the implementation is stable.
  Existing automatic rebuild and build-tag deployment are reused; no manual wire-version/build transfer should be required.
- Re-run the release/package checks appropriate to the final changes.
  Do not publish or push without the user's authorization.

## Useful commands after work is resumed

```sh
env RUSTC_WRAPPER= mise x -- cargo build --locked -p domyjob
env RUSTC_WRAPPER= mise run lint
env RUSTC_WRAPPER= mise run test
env RUSTC_WRAPPER= mise run deny
env RUSTC_WRAPPER= mise run audit
env RUSTC_WRAPPER= mise run vet
env RUSTC_WRAPPER= mise run check:fleet
env RUSTC_WRAPPER= mise run fuzz
```

On this Mac, taplo inside the restricted sandbox failed with a system-configuration error.
The successful full lint run used ordinary host permissions through the approval mechanism.

Do not run the preserved E2E draft against real provider binaries or the default user state.
It is intended to prepend fake executables to a temporary PATH and set an isolated `DOMYJOB_STATE`.
No provider was invoked and no chat test worker was left running when this checkpoint was stopped.

## Existing rewrite limitations that still apply

- On Unix, a descendant that leaves the job's process group while holding its output open keeps even a cancelled job from finishing until that output closes.
- There is no old-state migration, separate network service/pairing layer, or remote file pullback in the rewritten job runner.
- Old build versions are retained so active workers can finish; automatic cache pruning is not implemented here.
- Local Windows source refresh avoids replacing its own running output path, but contention with another process holding the alternate/primary build output has not been exhaustively tested.

These limitations were not reopened as implementation tasks during the chat checkpoint.
Revisit them only as part of a concrete resumed scope.
