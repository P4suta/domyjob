# Require source-bound verification receipts

Status: Proposed.

## Context

A successful solver process does not establish that the expected production contracts were checked.
An incomplete report, stale source, unreachable cover, or unrelated failure can otherwise produce a false success.
The durable job protocol also needs a checked model boundary that distinguishes finite trace agreement from a complete implementation refinement.

## Decision

The [authoritative inventory](../../verification/obligations.json) names every production proof, required cover, deliberate false assertion, and native semantic boundary.
Discovery must agree with that inventory, and complete structured reports must identify the expected checks individually.
Negative controls require their exact named assertion counterexample rather than a tool error or another failed property.
The gate fingerprints the implementation, dependencies, configuration, models, and required corpus before and after execution.
A source change refuses its receipt.

Kani checks the actual Rust decisions under the recorded domains and bounds.
The [Quint protocol](../../verification/job_protocol.qnt) checks initialization, inductive preservation, and its transition contract through pinned Apalache.
Indexed model traces must additionally replay through the production Rust step and exercise every action.
Trace agreement remains evidence for those finite executions; it does not establish infinite-trace refinement or liveness.
Native operating-system contracts and finite Miri executions retain the obligations outside the solver's supported semantics.

Reject summary-only proof acceptance because it loses individual assertion and cover identity.
Reject a detached model as the sole implementation check because a valid model can accompany a broken adapter.
Reject running another solver without a distinct obligation because agreement between tools cannot repair a missing production link.

## Consequences

Each implementation change must update its applicable inventory, native contracts, and required corpus together.
Failing tools and incomplete work remain visible instead of being relabeled as verification.
Fresh outputs and source checks add local and CI cost, while [strict report fixtures](../../xtask/src/resource_proofs.rs) and [model fixtures](../../xtask/src/verification.rs) protect the gate's own failure behavior.
New verifier support or a counterexample at a recorded native boundary requires reassessing the corresponding alternative.
