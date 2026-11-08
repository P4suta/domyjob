# Require source-bound verification receipts

Status: proposed.

## Context

Solver success alone can conceal stale sources, missing contracts, unreachable covers, or unrelated negative-control failures.

## Decision

Make the [proof inventory](../../verification/obligations.json) authoritative.
Require exact discovered obligations, individual assertions, reachable covers, and named counterexamples for false controls.
Fingerprint implementation, dependencies, configuration, models, and corpus before and after execution.
Refuse changed sources.
Check production Rust decisions with Kani.
Check Quint initialization and preservation, then replay indexed traces through production with every action exercised.

## Alternatives

Summary-only acceptance loses assertion identity.
A detached model can pass beside broken production code.
Another solver without a distinct obligation can't repair a missing implementation link.

## Consequences

Inventory maintenance and fresh runs add verification cost.
Finite replay establishes agreement only for exercised traces.
Native contracts and finite Miri runs cover supported boundaries without claiming complete refinement or liveness.

## Confirmation

Require [strict report and model contracts](../verification/proofs.md) to reject stale, missing, incomplete, and unrelated results.
Keep proposed pending applicable checks and owner acceptance.
Reconsider after a native-boundary counterexample or new verifier support.
