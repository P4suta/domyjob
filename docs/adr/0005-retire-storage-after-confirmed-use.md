# Retire storage after confirmed execution and delivery

Status: proposed.

## Context

Count-based pruning can discard unread results.
Keeping complete jobs wastes build storage.
Worker death alone provides no confirmation of process-tree quiescence.

## Decision

Apply the [storage lifecycle](../../crates/domyjob-core/src/storage_lifecycle.rs): retire workspaces only after terminal state and native quiescence.
Keep results and logs until successful requested output and acknowledgment.
Honor `--keep`.
Persist namespace-bound completion receipts before retiring records to preserve retries and deduplication.
Keep compatible-format provenance.
Retire superseded caches only after native leases and durable claims release.
Compare complete identities before mutation.
Protect installed builds through publication and first use with selection and usage locks.
Cleanup uses retained private directory handles without modifying link targets.
Refusals leave recoverable leftovers.

## Alternatives

Count or age limits lose unread results.
Retaining everything avoids deletion but grows storage without a use boundary.
PID-only cleanup can't establish ownership or quiescence.

## Consequences

Receipts remain durable execution history.
Locks, compatibility records, and uncertain cleanup keep storage allocated.

## Confirmation

Require [lifecycle proofs and native contracts](../verification/proofs.md) for delivery, quiescence, ownership, compatibility, and failure schedules.
OS behavior remains an adapter assumption.
Keep proposed pending checks and owner acceptance.
Reconsider after a native contract fails.
