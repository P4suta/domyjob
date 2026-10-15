# Notification contracts

Notification receipts are scoped to an exact message ID and recipient.
The ordered stages are pending, delivered, read, and acknowledged.
Observing a receipt joins it with the stored stage, so duplicate, delayed, or lower-stage receipts never regress the projection.
The sender's stage cannot advance another recipient's stage.
Normal messages enter context at a lifecycle hook.
An urgent recipient's stop decision is blocked until acknowledgment; a reply is an acknowledgment.

Expected-answer questions admit delays from one to 600 seconds and a configured fallback.
Elapsed time is checked by subtraction, so clock rollback and addition overflow cannot manufacture a deadline.
Answered and already escalated questions do not produce a fallback.
The fallback message and its original-to-fallback link commit atomically.
Hook presence becomes stalled at the configured elapsed bound.
Context admission uses checked byte addition and refuses messages beyond the summary budget.
Each summary bounds sender and conversation previews separately so maximal legal names cannot remove its subject.
The exact message ID and full-body read command remain present, with a reserved sixty-four-byte body preview budget inside the 512-byte summary bound.

## Evidence and limits

`chat/notification_proofs.rs` imports the production decisions.
Kani checks all receipt stages, arbitrary deadlines and timestamps, arbitrary byte budgets, and every urgency and activity variant.
The expected inventory contains five positive harnesses with reachable cover properties.
`verification/chat-negative.rs` imports the production stop decision and deliberately asserts that an unacknowledged urgent message allows stopping.
The gate requires its `NEGATIVE_CHAT_CONTROL` assertion counterexample rather than accepting a tool error.
The authoritative commands and source-bound receipts are maintained by `mise run verify` and `verification/obligations.json`.

The three-store model and `chat_ledger` fuzz target import production admission and receipt-join rules.
The preserved notification corpus exercises asking, exchange, each receipt stage, and activity exchange.
This finite replay and bounded exploration supplement the native decision proofs; they do not establish all possible distributed executions.

Native redb fixtures exercise two-machine receipt synchronization, distinct recipient state, duplicate receipts, reply acknowledgment, exact-deadline fallback, one-time escalation, and preservation of previous-format history.
An actual serde-to-session fixture verifies that an empty recipient route cannot discard invalid expected-answer or fallback settings, while a valid ordinary self-only note remains accepted.
Hook fixtures exercise bounded Japanese text, the delivery/read/acknowledgment sequence, directory presence, and urgent stop control.
A failing-output fixture verifies that a broken pipe leaves a newly selected message pending.
Receipt fault fixtures abort staged delivery, read, and acknowledgment transactions and inject actual redb backend write and durability-sync errors.
A refused backend write leaves the receipt and sequence unchanged after reopening.
A sync error can leave the old state or the complete new transaction, so the fixture checks that receipt, sequence, and urgent indexes agree and that retry appends at most one receipt.
An escalation interrupted by counter exhaustion leaves no fallback or link; reopening after repair creates one fallback and repeated escalation preserves its exact ID.
These checks use the production store and command adapters without launching an AI client.

The proof trusts Rust's checked string boundaries and the standard library's UTF-8 contracts when producing a summary prefix.
The client must run the configured synchronous hook, honor its context and stop output, and inherit the bound identity.
A successful write proves that the adapter returned context, not that the client consumed or understood it.
A subsequent agent action is the observable reading signal.
Client lifecycle semantics, redb durability, OS locking, SSH behavior, clock synchronization, and whether a stalled process resumes remain external boundaries.
Native client and machine checks are required for those supported environments, and no claim of verification follows from a missing or unsupported check.
