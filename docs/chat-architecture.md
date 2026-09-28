# Chat architecture

Chat uses the same OpenSSH connection and automatic node deployment as job execution.
Each machine stores an ordered event log in its private redb database.
Chat messages remain content; sending a message never submits a shell command.

## Invariants

| Invariant | Mechanical enforcement |
| --- | --- |
| Every origin has one ordered log | Transactions assign sequence numbers and Lamport clocks; imports reject gaps, clock regression, and conflicting duplicates. |
| Each message has one purpose | An exhaustive `MessageMode` enum distinguishes sends, asks, and replies. |
| Every machine displays the same order | Projections sort by `(clock, origin, sequence)`. |
| Private text reaches its audience only | A validated, sorted audience restricts synchronization; omitted events preserve sequence continuity. |
| AI sessions stay on their machine | Synchronized registrations redact working directories and session IDs. |
| A room has one membership authority | Only the room creator's origin can change membership. |
| An ask has at most one committed resolution | One transaction validates the original request, responder, conversation, and audience before committing its answer or failure. |
| An agent executes one managed turn at a time | An OS lock protects a durable claim through resolution. |
| Offline sends remain recoverable | A message commits locally before synchronization, and every result includes its synchronization outcome. |
| CLI and MCP use identical operations | Both map into one exhaustive action interpreter; a single tool declaration generates MCP schemas and dispatch. |

External AI execution and the database commit cannot share a transaction.
If a worker exits after claiming a turn, recovery records `interrupted` after acquiring the same agent lock.
It does not automatically repeat a turn whose external effects are unknown.

## Synchronization

`domyjob chat setup MACHINE...` records OpenSSH aliases and pins each peer's chat origin.
The reserved `local` alias refers to the current machine.
`owner@MACHINE` addresses a human participant without requiring an AI registration.
Unqualified AI names must resolve uniquely.
The name `owner` is reserved for human participants.

Each peer acknowledges the sender's origin sequence.
Reconnects resume after that durable acknowledgment; duplicate delivery is safe.
Participating machines synchronize directly with each machine whose events they need.
Operations synchronize before resolving names and projecting their result.
Writes also synchronize after their local transaction.
`ask`, `watch`, and `open` continue exchanging events while active.
`chat sync` performs a bounded exchange and reports each peer as `synced`, `partial`, or `failed`.
There is no always-running chat listener.

OpenSSH authenticates access to the operating-system account.
Chat runs within that account's trust boundary: its peers can send questions to registered managed agents.
Only register a managed agent when the peer accounts are trusted to request its work.

## AI adapters and MCP

Managed Claude Code, Codex, and OpenCode agents execute structured CLI turns and retain their session IDs locally.
An attached interactive session reads and replies through MCP or the CLI on its next turn.
Use `DOMYJOB_CHAT_AGENT` or `--from` to select the local sender.
The default sender is the machine owner.
The inbox selects messages for that participant, including conversations with other agents on the same machine.

`domyjob chat setup` prints an MCP configuration for `domyjob mcp`.
Copy that configuration into the AI client's MCP settings.
Setup does not modify third-party configuration files.
The MCP tools expose the same registration, rooms, inbox, thread, send, ask, and reply actions as the CLI.

`ask --timeout SECONDS` waits up to 600 seconds for a durable resolution, checking again after each synchronization exchange.
An ongoing SSH request can extend the wall-clock wait beyond the requested timeout.
It returns exit code 0 for `answered`, 1 for `failed` or `interrupted`, and 3 for `pending`.
A pending request remains stored and can be answered later.
Use `--json` to consume structured results, including message IDs and synchronization outcomes.

The external protocols are documented in the [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference), [OpenCode CLI reference](https://opencode.ai/docs/cli/), and [MCP stdio transport specification](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports).
