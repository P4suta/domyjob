# Source-bound verification

`mise run verify` owns the [proof inventory](../../verification/obligations.json).
This head registers six positive Kani harnesses, three named negative controls, and eighteen required covers.
Every obligation records its contract, admitted inputs, bound, and production entry point.
Discovery must equal the exact registered harness set in the core and standalone sources.
An absent source, unregistered proof, duplicate identity, empty phase, stale inventory, skipped cover, solver error, or unrelated negative failure refuses the gate.
An optional standalone source selects that maintained source explicitly; omission selects the core Cargo lane and never makes execution optional.

Kani 0.68.0 checks the production execution-result, resource-budget, job-state, persistent-protocol and source-admission decisions.
Execution outcomes quantify every `i32` exit code with a fixed valid launch-error payload and four reachable outcomes.
Resource admission quantifies every `u32` concurrency count and `u64` memory value with two covers.
The job-state proof checks the finite phase and event variants, arbitrary PID and exit-code inputs, a representative prior nonzero PID, and a one-byte launch reason with unwind sixteen and three covers.
The persistent-protocol proof checks every durable state and action, including both request-equivalence values, without loops or input assumptions and with four covers.
The resource negative must fail its `NEGATIVE_RESOURCE_CONTROL` assertion, and the disconnect-loss negative must fail its `NEGATIVE_JOB_CONTROL` assertion.
Complete reports must contain every harness, unique check identities, successful assertions, and the exact individually satisfied cover count.
Unwinding failures and incomplete result summaries cannot pass.

The persistent job protocol projects queued, starting, and running phases into an active durable state.
The production store uses `Submit` while retaining its existing admission locks and complete request equality.
The launch adapter uses `Attach` only after acquiring its existing launch lock and observing the stored state.
An observed dead active worker uses `Reap` before recording supervisor loss.
These decisions retain duplicate submissions, reject conflicting submissions, and preserve durable state across client disconnect.
Process creation, filesystem synchronization, and kernel ownership remain fallible native adapter operations.

Quint 0.32.0 uses pinned Apalache 0.56.1 to check initial establishment, inductive preservation, and the transition-contract implication over the model's finite domains.
The model contains the five actual protocol actions: same-request submission, conflicting submission, attachment, disconnect, and reaping.
One hundred seed-one traces of twenty transitions replay every indexed action through the actual Rust `job_protocol::step` function.
Every action must appear, and a false invariant probe must produce a model counterexample rather than a tool failure.
Trace replay establishes agreement on those finite executions; it does not prove infinite-trace refinement or liveness.
Node 26.11.1 and Temurin Java 25.0.4+101.0.LTS are pinned prerequisites.
The [Java catalog provider](java-catalog.md) preserves mise's exact Temurin aliases and the shared minimum-age policy when the default Adoptium lookup is unavailable.

The fuzz phase copies exactly three checked-in wire seeds into a fresh replay directory: `hello`, `chat-exchange`, and `chat-unknown-field`.
It also copies the checked-in source seeds `tracked`, `metadata` and `malformed` into a separate fresh replay directory.
Missing or empty required corpus files fail before libFuzzer starts.
The maintained cargo-fuzz 0.13.2 targets replay those six inputs through production wire and source ingress with zero mutation runs and seed one.
Exploratory corpus output cannot change the required replay set.

`mise run miri` interprets five finite production state, protocol, refusing generic-serialization and source-ingress lanes through nightly-2026-10-01.
Every named lane requires nonempty test execution.
The serializer fixture exercises the public generic `frame` function with a valid custom serializer that returns its own error and requires that exact error without a panic.
Miri establishes its checks for those concrete executions under supported memory semantics; it does not establish soundness for every caller or schedule.

The source budget proof checks the production per-byte transition for every byte and all admitted counters, requiring reachable byte admission, empty-record refusal and unterminated-record refusal.
Its counter domain preserves the existing 100,000-file and 16,777,216-path-byte archive limits, with a 4,096-byte record bound and a 16,877,216-byte tracked stream bound.
The metadata proof selects all sixteen fixed root/nested paths for the eight production metadata names and both ordinary/metadata controls, with unwind sixteen and unwinding assertions.
The named false metadata control imports the exact production classifier and must fail `NEGATIVE_SOURCE_CONTROL`.
These proofs do not establish all UTF-8 or Unicode paths, native Git output, filesystem capability semantics or an atomic view of concurrently changing files.
Native Git fixtures include ignored tracked working bytes, ignored untracked exclusion, tracked deletion, metadata exclusion and subdirectory-relative paths.
The global-ignore fixture creates its real configuration with Git's argument-based writer, and native link/junction fixtures refuse reading outside the retained source root.
Runtime and build-time archive admission use that same capability-relative boundary without changing portable-path, case-collision or snapshot-byte checks.
The small parser and metadata interpreter fixtures remain separate from the large native record/count/aggregate boundary tests.

The gate fingerprints production source, locked dependencies, proof sources, configuration, the model, and the exact required corpus before and after execution.
Only an unchanged source may produce its success receipt.
Results from a larger feature set or a different checkout do not certify this baseline.
Linux and Mac own the complete verifier and interpreter lanes.
Windows requires the same nonempty exact inventory, native SDK contracts, and native workspace tests; inventory consistency alone is not a proof result.
Native obligations and their reconsideration conditions are recorded beside the proof inventory.
The arithmetic, transition, and protocol proofs do not establish SSH reliability, native process ownership, operating-system infallibility, or behavior of unreviewed dependencies.
