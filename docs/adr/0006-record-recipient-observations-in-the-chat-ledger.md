# Record recipient observations in the chat ledger

Status: superseded by the [chat retirement decision](retire-agent-chat-and-retain-job-service.md).

## Context

Machine delivery doesn't establish agent reading.
Global cursors lose independent recipient state and late messages.

## Decision

Record delivery, reading, and acknowledgment per exact message and recipient fixed at creation.
The historical [receipt join](https://github.com/P4suta/domyjob/blob/d663df1fb119a13083a7946280da21fd64ce5829/crates/domyjob-core/src/chat/notification.rs) preserves the greatest stage.
Replies acknowledge their referenced message.
Successful hook output records delivery.
The next invocation records reading.
Urgent stops require a reply or acknowledgment.
The sending machine atomically commits one fallback ask and link after a checked persisted deadline.
Clock rollback refuses escalation.

## Alternatives

Machine acknowledgments establish storage only.
Treating received messages as read hides recipient actions.
Delivering context exactly once requires an unavailable atomic transaction across output and storage.

## Consequences

Retries can repeat context after a crash between output and receipt.
Observations don't establish model understanding.
Client lifecycle, transport, durability, and clocks remain external assumptions.

## Confirmation

Require historical [notification proofs and contracts](https://github.com/P4suta/domyjob/blob/d663df1fb119a13083a7946280da21fd64ce5829/docs/verification/notifications.md), named negative controls, store faults, distributed exploration, and native client checks.
Retirement supersedes the unconfirmed proposal.
Reconsider if a client provides a causal consumption receipt or a lifecycle fixture fails.
