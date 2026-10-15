# Source-bound verification

`mise run verify` owns the [proof inventory](../../verification/obligations.json).
The inventory defines the required positive Kani harnesses, named negative controls, and cover counts.
Every obligation records its contract, admitted inputs, bound, and production entry point.
Discovery must equal the exact registered harness set in the core and standalone sources.
An absent source, unregistered proof, duplicate identity, empty phase, stale inventory, skipped cover, solver error, or unrelated negative failure refuses the gate.
An optional standalone source selects that maintained source explicitly; omission selects the core Cargo lane and never makes execution optional.

`cargo xtask verify parent-identity` is a closed native Linux calibration.
It validates the full inventory and the actual Parent source refinement, then runs the imported proof-controller exit policy before the complete Parent identity pair.
The exit policy quantifies both proof phases and every optional i32 code with four covers and `NEGATIVE_PROOF_EXIT_CODE` as the false admission claim.
The Parent proof quantifies every canonical 128-bit origin and nonzero u64 sequence independently for parent and request, with six identity and classification covers and `NEGATIVE_PARENT_IDENTITY` as its false claim.
The actual runner requires normal zero for positive proofs and normal one for named counterexamples before it reads the unchanged strict report decoder.
Its native-default arguments use one proof job, two build jobs, the unchanged sixty-second harness budget, and positive-only fail-fast.
Source-after binding runs even after proof refusal and retains both failures when needed.
Each proof captures the actual selected Kani proxies, mise and Cargo launchers, linked compiler and toolchain, required backend roles, runtime support files, and exact child version observations before execution.
The independent after observation still runs after admission, compiler, process, or report refusal and retains both causes when they fail.
Owned evidence directories retain the raw report and tool observations after the command returns.
Implementing these checks does not supply actual native qualification or Parent proof results.
This scoped calibration does not certify all registered proofs, other platforms, notice admission, receipt authorization, Question delay, or hook formatting.

`cargo xtask verify parent-identity-component generator|present|absent` selects exactly one of three additive Parent components for native Linux calibration.
Each invocation validates the full inventory and seven-file actual source refinement, runs the required exit-policy pair first, and then runs the selected registered component with the unchanged `NEGATIVE_PARENT_IDENTITY` control.
Unknown names, arbitrary harnesses, source paths, extra arguments, and solver or deadline overrides refuse before proof execution.
The generator component checks the real full-domain factory and every preserved Origin byte, with one additional minimum-sequence reachability cover.
Present checks exact complete identity equality and its four origin/sequence classes.
Absent checks both Opaque and Missing identity decisions with two separate covers; it does not authorize a missing Ledger dependency.
Present and Absent each retain the original two real full-domain identity generators and both byte-preservation assertions.
The one-call generator component provides additional diagnostic evidence rather than a constructor summary assumed by either decision component.
The existing exact normal-code policy, cover and assertion admission, default proof flags, resource bounds, and independent source-after check remain required.
The physical native tool/configuration binding is enforced for each proof as in the aggregate calibration.
The original aggregate harness and its six covers remain registered and required by `parent-identity` and full verification.
Individual component success cannot complete that aggregate.
No checked composition or derived-obligation substitution is implemented.

`cargo xtask verify parent-identity-encoder` selects one proof-only arithmetic hexadecimal encoder theorem and its independent `NEGATIVE_PARENT_HEX_ENCODER` control after the required exact exit-policy pair.
It retains the full inventory, seven-file Parent refinement, actual tool binding, and independent source-after checks.
The theorem quantifies every u8 input, proves exact equality to the original sixteen-byte lookup, proves the lowercase hexadecimal range, and supplies an inverse for every valid output byte without assumptions.
Two covers require the zero digit and final lowercase letter to remain reachable.
The factory maps all thirty-two independent u8 choices through the pointwise-equivalent arithmetic encoder, a byte-preserving ASCII mask, checked typed ASCII construction, Origin validation, and EventId construction.
The separate encoder theorem supplies no factory or aggregate completion by itself.
Factory and aggregate success each require their own complete report with every byte-preservation assertion and expected cover.

`cargo xtask verify native-default` selects only the fixed tool-qualification inventory after the complete source inventory and Parent refinement checks.
It requires five positive harnesses, five exact named counterexamples, and twelve covers for the actual imported native-default kernel.
Those obligations cover both supported object-header representations, full-width member and byte budgets, every required role, every identity field and digest byte, the builtin configuration, and the pinned loader-tail filter.
Unknown names, source substitutions, missing or duplicated obligations, altered cover counts, and changed negative assertions refuse the selector.
The ordinary proof command uses the pinned native-default backend and command-local loader isolation; standalone sources receive the same Rust 2024 edition as the workspace.
The report must agree with its actual target and observed tools, exactly one builtin Cadical solver with an explicit null version, sixteen object bits, complete unique harness observations, full property counts, and the required clean or PanicsOnly assertion-failure phase.
Tool snapshots cap each member at 256 MiB, all member bytes at 1024 MiB, and member count at 1024; the 16 MiB report and existing source-transfer bounds remain separate.
Version probes retain the ten-second deadline and one-MiB diagnostic bound, and actual proofs retain the sixty-second per-harness budget and one solver job.
Native paths, filesystem identity, SHA-256 implementation, process execution, and the operating system remain adapter trust boundaries, with specific replacement, alias, loader, identity, and failure controls.
Current-source native execution is required before this qualification can be reported as passed.

Kani 0.68.0 checks the production execution-result, resource-budget, job-state, persistent-protocol and source-admission decisions.
Execution outcomes quantify every `i32` exit code with a fixed valid launch-error payload and four reachable outcomes.
Resource admission quantifies every `u32` concurrency count and `u64` memory value with two covers.
The standalone outbound proof imports the actual transport writer and quantifies every byte in three two-byte segments, every partial acceptance count from zero through six, and both flush outcomes, with unwind eight and five covers.
It checks the exact accepted prefix, first-error refusal, and final-flush result; the false partial-write success control must fail `NEGATIVE_OUTBOUND_CONTROL`.
Native helper fixtures separately exercise interrupted retries, zero writes, empty segments, and flush failures.

The standalone input-controller proofs import the actual Windows fixture controller.
They quantify every fixed-size case/process token, every scalar clock/budget value, typed Ready and child observations, and all failure choices in the stated three-observation port.
Seventeen covers require representation boundaries, actual readiness admission/refusals, fixed operation and case deadlines, clock regression, and retained primary/cleanup outcomes.
Three exact false controls reject completion at the operation deadline, foreign Ready admission, and discarded owned cleanup errors.
The actual parent records the operation start before publishing Begin; PowerShell publishes Ready only after actual Add-Type and its zero/invalid-count checks.
Every pending, zero-byte, and completion observation shares the unchanged five-second operation start, and the existing whole-case command budget remains authoritative.
These source-bound contracts do not establish PowerShell compilation time or Win32 Job behavior.
Native controls retain all eight binary input schedules and exercise a held descendant in the exact creation-time Job, followed by owned termination, root reap, and zero-active-process observation.
The original hosted failure's phase remains unestablished; a successful rerun alone is not a causal fix.
The actual reaper CLI uses the typed process-group constructor, whose proof quantifies every `i32`, preserves every admitted positive value, and covers positive, zero, and negative inputs.
Its false ownership control must fail `NEGATIVE_PROCESS_GROUP_CONTROL`.
Native text fixtures preserve the exact prior integer parsing, error text, and canonical round trips; the mandatory process-group corpus replays positive, nonpositive, and overflow ingress.
The job-state proof checks the finite phase and event variants, arbitrary PID and exit-code inputs, a representative prior nonzero PID, and a one-byte launch reason with unwind sixteen and three covers.
The persistent-protocol proof checks every durable state and action, including both request-equivalence values, without loops or input assumptions and with four covers.
The resource negative must fail its `NEGATIVE_RESOURCE_CONTROL` assertion, and the disconnect-loss negative must fail its `NEGATIVE_JOB_CONTROL` assertion.
The five notification harnesses check the actual recipient join, urgent-stop decision, elapsed deadline, activity expiry, and checked context admission with seventeen required covers.
Their complete scalar domains include all receipt stages, urgency and activity variants, arbitrary u64 timestamps, and every usize budget.
The hook compaction harness checks the actual retained-group ordering over at most four arbitrary groups with three covers.
The complete-output capture harness checks every u64 omitted-byte count and both production policies with four covers.
Their named false claims must fail `NEGATIVE_CHAT_CONTROL`, `NEGATIVE_HOOK_CONTROL`, and `NEGATIVE_TOOL_CAPTURE_CONTROL` respectively.
Complete reports must contain every harness, unique check identities, successful assertions, and the exact individually satisfied cover count.
Unwinding failures and incomplete result summaries cannot pass.

The worker handoff harness imports the production ordered request-port function.
It quantifies all four Boolean refusal choices and every eight-bit owned cause with no input assumptions and unwind eight.
Five covers require success and the first failure at Read, Decode, Validate, and Launch.
The exact negatives reject relabelled request-read failure and discarded error payloads.
The actual Mac adapter supplies bounded reads, JSON ingress, path and environment validation, and native launch; those parser and operating-system semantics are not replaced by the port proof.
Native controls retain injected error kind, operating-system code, and cause, require zero launches before valid admission, and exercise an actual partial-request timeout with the unchanged thirty-second production bound.
The observed intermittent timeout's underlying EOF or scheduling cause remains unestablished.

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

The fuzz phase copies exactly three checked-in wire seeds into a fresh replay directory: `hello`, `chat-exchange`, and `chat-unknown-field`.
It also copies the checked-in source seeds `tracked`, `metadata` and `malformed` into a separate fresh replay directory.
Missing or empty required corpus files fail before libFuzzer starts.
The checked-in process-group seeds `positive`, `nonpositive`, and `overflow` add a third isolated replay directory.
The checked-in `chat_ledger/notifications` seed and `hook_clients/foreign` and `hook_clients/malformed` seeds add three nonempty receipt/activity and hook-document inputs.
The maintained cargo-fuzz 0.13.2 targets replay those twelve inputs through five production ingress and model targets with zero mutation runs and seed one.
Exploratory corpus output cannot change the required replay set.

`mise run miri` interprets fourteen finite production state, protocol, refusing generic-serialization, source-ingress, process-group, segmented-writer, worker handoff, login reply, input-controller, notification, hook-merge, and capture-policy lanes through nightly-2026-10-01.
Every named lane requires nonempty test execution.
The serializer fixture exercises the public generic `frame` function with a valid custom serializer that returns its own error and requires that exact error without a panic.
The login reply target imports the production adapter and checks partial writes, both failure outcomes, interrupted progress, write zero, and empty replies in four finite tests.
The worker handoff core test checks owned error preservation, exact stage order, admitted request forwarding, and each refusal phase through the production function.
Miri establishes its checks for those concrete executions under supported memory semantics; it does not establish soundness for every caller or schedule.

The [notification contracts](notifications.md) and [client hooks](client-hooks.md) record ledger/store linkage, output-before-delivery ordering, client trust, and the limits of those checks.

Bootstrap sends the bounded source archive, unchanged original frame, and snapshot payload over one connection.
The exact source prefix is consumed before a private installer runs with isolated stdin and diagnostic stderr; the first node inherits the untouched remainder and EOF.
Publication holds the installed-build selection lock until final usage admission, and the publisher retains that usage lock through the first node.
The existing two-idle-build retention policy remains unchanged.
Native fixtures make an unprotected candidate eligible using two newer idle companions and compare its deletion with protected publication and first use.
The same scripts exercise truncated input and primary-preserving first-node, cwd-recovery, and cleanup faults.
Windows tests run actual inherited overlapped pipe schedules in Windows PowerShell and PowerShell 7, while CLR marshalling, native completion, shell inheritance, filesystem identity, ACLs, and kernel locks remain external semantic boundaries.
The Rust segmented proof does not establish those native effects.

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
