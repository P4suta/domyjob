# Source-bound verification

`mise run verify` owns the [proof inventory](../../verification/obligations.json).
This baseline registers five positive Kani harnesses, four named negative controls, and seventeen required covers.
Every obligation records its contract, admitted inputs, bound, and production entry point.
Discovery must equal the exact registered harness set in the core and standalone sources.
An absent source, unregistered proof, duplicate identity, empty phase, stale inventory, skipped cover, solver error, or unrelated negative failure refuses the gate.
An optional standalone source selects that maintained source explicitly; omission selects the core Cargo lane and never makes execution optional.

Kani 0.68.0 checks the production execution-result, resource-budget, job-state, and persistent-protocol decisions.
Execution outcomes quantify every `i32` exit code with a fixed valid launch-error payload and four reachable outcomes.
Resource admission quantifies every `u32` concurrency count and `u64` memory value with two covers.
The job-state proof checks the finite phase and event variants, arbitrary PID and exit-code inputs, a representative prior nonzero PID, and a one-byte launch reason with unwind sixteen and three covers.
The persistent-protocol proof checks every durable state and action, including both request-equivalence values, without loops or input assumptions and with four covers.
The resource negative must fail its `NEGATIVE_RESOURCE_CONTROL` assertion, and the disconnect-loss negative must fail its `NEGATIVE_JOB_CONTROL` assertion.
Complete reports must contain every harness, unique check identities, successful assertions, and the exact individually satisfied cover count.
Unwinding failures and incomplete result summaries cannot pass.

The login reply harness imports the production generic write-and-shutdown adapter.
It checks every byte in payloads of length zero through four, every stable one-byte write cutoff within that payload, and both shutdown outcomes with unwind eight.
Four covers require empty input, nonempty success, an incomplete write, and a shutdown failure.
The two named negatives reject shutdown after an incomplete write and success after a shutdown failure.
The proof establishes ordered byte preservation and exact error outcomes under that writer contract.
It doesn't establish arbitrary writer implementations, native socket behavior, concurrency, or launchd.
The Mac native controls keep an actual duplicate receiver handle open while requiring both readiness and refusal replies to finish.

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

The fuzz phase copies exactly three checked-in wire seeds into fresh replay directories: `hello`, `chat-exchange`, and `chat-unknown-field`.
Missing or empty required corpus files fail before libFuzzer starts.
The maintained cargo-fuzz 0.13.2 target replays those inputs through production wire ingress with zero mutation runs and seed one.
Exploratory corpus output cannot change the required replay set.

`mise run miri` interprets finite production state, protocol, refusing generic-serialization, and login reply contracts through nightly-2026-10-01.
Every named lane requires nonempty test execution.
The serializer fixture exercises the public generic `frame` function with a valid custom serializer that returns its own error and requires that exact error without a panic.
The login reply target imports the production adapter and checks partial writes, both failure outcomes, interrupted progress, write zero, and empty replies in four finite tests.
Miri establishes its checks for those concrete executions under supported memory semantics; it does not establish soundness for every caller or schedule.

The gate fingerprints production source, locked dependencies, proof sources, configuration, the model, and the exact required corpus before and after execution.
It includes the entire verifier package, including its manifest and any Cargo automatic build inputs.
Only an unchanged source may produce its success receipt.
Results from a larger feature set or a different checkout do not certify this baseline.
Linux and Mac own the complete verifier and interpreter lanes.
Windows requires the same nonempty exact inventory, native SDK contracts, and native workspace tests; inventory consistency alone is not a proof result.
Native obligations and their reconsideration conditions are recorded beside the proof inventory.
The arithmetic, transition, and protocol proofs do not establish SSH reliability, native process ownership, operating-system infallibility, or behavior of unreviewed dependencies.
