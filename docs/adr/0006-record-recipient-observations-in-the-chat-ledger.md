# Record recipient observations in the chat ledger

Status: Proposed.

## Context

Copying a message to a machine does not establish that an agent received it in context.
A global read cursor cannot represent independent recipients or a late message that sorts before an earlier read.
Urgent attention needs a completion decision based on the exact recipient's response.
Questions need a restart-safe deadline and one identifiable fallback action.
Concurrent hooks and exchanges must preserve receipts without depending on arrival order.

## Decision

Record delivery, reading, and acknowledgment as recipient-scoped ledger observations.
The [production receipt join](../../crates/domyjob-core/src/chat/notification.rs) selects the greatest observed stage for an exact message and recipient.
A reply acknowledges its referenced message for its author.
Explicit notification recipients are captured when a message is authored; room membership changes do not transfer an existing receipt to another agent.

The [hook adapter](../../crates/domyjob/src/chat/hook.rs) returns bounded context and the command for reading a full body.
The command records delivery only after a successful output write.
A subsequent pre-tool invocation records a reading observation for already delivered messages.
That observation describes call order, including parallel calls, rather than proving that a model understood the context.
An urgent stop remains blocked until the recipient replies or explicitly acknowledges the exact message.

Expected answers and wall-clock creation stamps travel with the question.
The author machine alone commits a fallback ask and its original-to-fallback link in one transaction.
The [deadline decision](../../crates/domyjob-core/src/chat/notification.rs) uses checked elapsed subtraction and refuses clock rollback.
Persisted timestamps support restarts; machine ordering continues to use the ledger's origin sequence and Lamport clock.

Activity events publish the latest observed hook call and status.
Presence labels become stalled and offline after the elapsed threshold, without claiming to inspect an idle client's process.
The immediately preceding additive store format upgrades without deleting its identity or events.

## Assurance boundary

Source-bound Kani harnesses prove receipt joins, urgent stop decisions, deadline decisions, activity expiry, and context byte admission over their complete scalar input domains.
The required false urgent-stop assertion must produce its named counterexample.
The [notification contract evidence](../verification/notifications.md) separates these proofs from distributed exploration, store fixtures, and client checks.

Output writes and database commits span separate resources.
A crash after output but before its receipt can cause context to be delivered again, while a failed output cannot advance a newly selected message.
Receipts remain idempotent under those retries.
Client trust settings, lifecycle execution, context insertion, OS locks, SSH transport, database durability, and wall-clock synchronization remain external assumptions.
Revisit the reading observation when a supported client exposes a causal context-consumption receipt or a native fixture shows a lifecycle mismatch.

## Consequences

Senders can distinguish pending, delivered, read, and acknowledged recipients after exchange.
Late receipt observations and repeated delivery cannot regress progress.
Urgent attention and fallback actions use the same bounded ledger rather than output files or a separate message broker.
A machine-level exchange acknowledgment was rejected as an agent reading signal because it establishes only transport storage.
Treating every received message as already read was rejected because it hides context delivery and recipient actions.
Exactly-once context delivery was rejected because no atomic transaction spans the client output stream and the database.
