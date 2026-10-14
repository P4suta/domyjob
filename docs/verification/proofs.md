# Source-bound verification

`mise run verify` owns the [proof inventory](../../verification/obligations.json).
This head registers nineteen positive Kani harnesses, five named negative controls, and forty-one required covers.
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

`mise run miri` interprets six finite production state, protocol, refusing generic-serialization and source-ingress lanes through nightly-2026-10-01.
Every named lane requires nonempty test execution.
The serializer fixture exercises the public generic `frame` function with a valid custom serializer that returns its own error and requires that exact error without a panic.
Miri establishes its checks for those concrete executions under supported memory semantics; it does not establish soundness for every caller or schedule.

The source budget proof checks the production per-byte transition for every byte and all admitted counters, requiring reachable byte admission, empty-record refusal and unterminated-record refusal.
Its counter domain preserves the existing 100,000-file and 16,777,216-path-byte archive limits, with a 4,096-byte record bound and a 16,877,216-byte tracked stream bound.
The metadata path proof selects all sixteen fixed root/nested paths for the eight production metadata names and checks both ordinary/metadata controls.
Eight casing proofs each quantify the same six independent Boolean case choices for one of the same eight names, with all-uppercase and all-lowercase covers for every name.
Their conjunction preserves the original combined input domain within the unchanged per-harness timeout.
The path and casing proofs retain unwind sixteen and unwinding assertions.
The named false metadata control imports the exact production classifier and must fail `NEGATIVE_SOURCE_CONTROL`.
The four standalone metadata-directory proofs import the actual typed component-prefix matcher used by the production collector.
Its native path adapter passes actual `Path::components()` into that same matcher, and finite Miri controls check complete components and rooted-versus-relative namespaces.
Each proof quantifies every three-byte lowercase ASCII custom directory name for one of the same four exact root, Git descendant, common descendant, and component-prefix sibling sequences with unwind thirty-two and one cover.
Their union preserves all 26^3 names across all four original sequences and all four original covers within the unchanged per-harness timeout.
On Kani's Unix target, the proof-only byte constructor uses safe `OsStrExt::from_bytes` for the same admitted ASCII names; native and Miri controls retain the separate `Path::components()` adapter boundary.
The deliberately false custom-directory delivery assertion must fail `NEGATIVE_GIT_DIRECTORY_CONTROL`.
This bounded predicate proof does not establish arbitrary native path parsing or filesystem case-folding semantics.
The read-length proof quantifies all expected and actual u64 lengths through the production admission, length+1 limit, and exact completion functions, with three reachable covers and `NEGATIVE_SOURCE_READ_CONTROL` as its false claim.
Actual archive and fingerprint readers apply that limit to the native reader and reject changed lengths.
Native no-follow, retained-parent, FIFO, and read-fault fixtures establish the adapter linkage; they do not prove operating-system behavior or an atomic source snapshot.
These proofs do not establish all UTF-8 or Unicode paths, native Git output, filesystem capability semantics or an atomic view of concurrently changing files.
Native Git fixtures include ignored tracked working bytes, ignored untracked exclusion, tracked deletion, metadata exclusion and subdirectory-relative paths.
The nested Git fixture forces an ancestor-ignored file into the index, changes its working bytes, and requires the snapshot to exclude the equally ignored untracked peer.
A second nested fixture compares actual Git with competing relative excludes files and repository-root anchored patterns while retaining changed tracked bytes.
Separate-directory fixtures require private Git and common metadata to remain excluded even when a metadata canary is forced into the index.
Individually bounded absolute Git-directory queries precede working-tree discovery so selected metadata and bare roots are refused.
Malformed ancestor declarations refuse collection, while an ordinary non-Git directory with the same custom name remains working input.
The merged candidate filter uses native canonicalization before the captured-directory predicate to bind case aliases, and the retained no-follow archive reader remains authoritative for file reads.
In Git mode, the actual bounded untracked-path query owns ancestor, global, `info/exclude`, and `.gitignore` precedence; `.ignore` walking cannot reinstate Git-ignored private input.
The native `info/exclude` control requires Git-ignored private input to stay absent while `.gitignore` negation and current tracked bytes remain present.
Canonical root discovery and ignore matching remain native boundaries; these checks do not prove an atomic relationship between Git queries and concurrently changing files.
The global-ignore fixture creates its real configuration with Git's argument-based writer, and native link/junction fixtures refuse reading outside the retained source root.
Runtime and build-time archive admission use that same capability-relative boundary without changing portable-path, case-collision or snapshot-byte checks.
The small parser and metadata interpreter fixtures remain separate from the large native record/count/aggregate boundary tests.

The gate fingerprints production source, locked dependencies, proof sources, configuration, the model, and the exact required corpus before and after execution.
It includes the entire verifier package, including its manifest and any Cargo automatic build inputs.
Only an unchanged source may produce its success receipt.
Results from a larger feature set or a different checkout do not certify this baseline.
Linux and Mac own the complete verifier and interpreter lanes.
Windows requires the same nonempty exact inventory, native SDK contracts, and native workspace tests; inventory consistency alone is not a proof result.
Native obligations and their reconsideration conditions are recorded beside the proof inventory.
The arithmetic, transition, and protocol proofs do not establish SSH reliability, native process ownership, operating-system infallibility, or behavior of unreviewed dependencies.
