# Bind continuing-agent handoffs to explicit readers

Status: superseded by the [chat retirement decision](retire-agent-chat-and-retain-job-service.md).

## Context

Project continuation must preserve the acting identity and disclose metadata before body access.
Changing room membership can't retroactively grant access.

## Decision

Publish immutable handoffs with project, scope, byte count, intended recipient or team, and named reader grants.
Exchange bodies only with the sender and explicitly granted readers.
Teams describe intent.
Explicit identities grant access.
Derive metadata-only discovery atomically without loading bodies.
Admit the acting reader, complete descriptor, and byte bound before body lookup.
Require matching fetched content.
After successful output and flush, commit an idempotent actual-reader observation with identity and descriptor rechecks.
Use shared typed CLI and MCP operations.

## Alternatives

Assuming the previous recipient's identity exposes its inbox.
Dynamic room grants permit unintended readers.
Delivering output exactly once can't span stream and database transactions.

## Consequences

Crashes can repeat responses before observation commits.
Trusted origin registration, immutable storage, and durability remain adapter assumptions.
Observations establish emitted output, not model understanding.

## Confirmation

Require historical [handoff proofs and contracts](https://github.com/P4suta/domyjob/blob/d663df1fb119a13083a7946280da21fd64ce5829/docs/verification/handoffs.md), named false controls, metadata-only discovery, output faults, rollback, and preserved-format checks.
Retirement supersedes the unconfirmed proposal.
Reconsider after an identity, descriptor, output, or compatibility contract fails.
