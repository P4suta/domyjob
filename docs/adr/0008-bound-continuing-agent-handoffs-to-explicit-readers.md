# Bind continuing-agent handoffs to explicit readers

Status: Proposed.

## Context

Continuing project work needs scoped context without assuming a previous agent's identity or opening its private inbox.
Room membership can change, so intended recipients and teams cannot implicitly authorize future readers.
The acting agent needs metadata before deciding to fetch a body.

## Decision

Publish immutable handoff events with explicit full agent identities as reader grants.
Bind each descriptor to the exact event, sender, intended recipient, project, scope, readers and UTF-8 body byte count.
Keep discovery in a metadata-only index and admit the exact selected descriptor and byte bound before body lookup.
Export placeholders to origins outside the sender and named-reader audience.
Keep handoff bodies outside ordinary thread, inbox and dashboard projections.

Record an observation for the actual acting reader only after successful output of the original body response.
The pending-observation capability retains the admitted descriptor and reader, and commit rechecks their identity and immutable binding.
The ledger validates the parent, explicit grant, issuer, exact audience and later Lamport clock.
CLI and MCP share the same typed arguments and operations.
The new format retains explicit compatibility with the original chat and notification formats during additive initialization.

## Consequences

Explicit grants prevent continuation from depending on impersonation or changing room membership.
Separate metadata discovery avoids unnecessary private body access.
Dynamic team grants and inherited inbox access are rejected because they would weaken immutable reader authorization.
Per-reader observations describe emitted output rather than comprehension, and a crash before commit can repeat a response.
Idempotent observation supports that retry without claiming exactly-once delivery.
Origin authentication, trusted local registration and stream/database durability remain adapter assumptions.
The [operating guide](../verification/handoffs.md) describes commands and required assurance without crediting unexecuted verification.
