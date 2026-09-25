# Threat model

domyjob exists to make other machines run code.
Whoever controls a domyjob connection controls those machines, so every boundary below is treated as a remote-code-execution boundary.

## Assets

- The ability to run commands as the user on every machine that trusts this one.
- Identity keys (`identity.json`) and the trust records (`trust.json`) that decide who may connect.
- Source trees, job output, and artifacts, which routinely contain secrets.
- The binaries themselves, which are copied to remote machines and executed there.

## Principals

| Principal | Trusted for | Not trusted for |
| --- | --- | --- |
| The local user | Everything on machines they configured | — |
| A paired client | Only what its grant allows on this machine | Anything outside that grant |
| A paired server | Running the jobs it was sent | The content of anything it sends back: logs, files, JSON, names |
| Repository content arriving through a trigger | Being built | Choosing where files land or what runs outside its job |
| The local network | Nothing | — |
| The release channel | Carrying signed artifacts | Vouching for artifacts on its own |

## Adversaries

1. **On-path attacker on the LAN or the internet** who can read, drop, reorder, and inject traffic, and answer mDNS queries.
2. **Unpaired host that can reach the listener**, attempting pre-authentication resource exhaustion or protocol confusion.
3. **Paired but compromised or malicious peer**, in either direction.
4. **Hostile repository content** delivered by a push trigger or by the snapshot of an untrusted directory.
5. **Compromised release infrastructure** or dependency.
6. **Another local user** on a shared machine reading or replacing state files.
7. **The operator's own mistake**: exposing a listener, pairing the wrong machine, granting too much, forgetting a revoked device.

## Security properties required

- **Mutual authentication with pinned keys** for every non-ssh connection, and authentication before any work is done.
- **Confidentiality and integrity with forward secrecy** for every byte after the handshake, with the connection purpose bound into the handshake transcript.
- **Pairing that resists offline guessing**, where an on-path attacker gets at most one online guess per attempt, and the offer closes after one success, a fixed number of attempts, or when `serve --pair` stops.
- **Least privilege**: a peer holds explicit capabilities, and every request is checked against them by an exhaustive match.
- **Revocation and rotation** that take effect on the next connection, and are visible.
- **Bounded pre-authentication cost** and bounded per-request resources.
- **Confinement of materialized files** to the job workspace regardless of manifest content.
- **Output neutralization**: nothing a remote sends can drive the local terminal or choose local paths.
- **Authenticated releases**: a binary is executed or pushed only after verification against a key the running binary already holds, and never downgraded silently.
- **An audit trail** on each node of who asked for what, and whether it was allowed.
- **Fail closed**: an unreadable trust file, an unknown field, an unexpected message, or an unverifiable artifact stops the operation.

## Costs accepted by design

Nothing in domyjob gives up on a peer because time passed, so a peer that connects and then says nothing keeps its connection.
What it costs is bounded by counts instead: a paired server admits a fixed number of connections overall and per source address, and a pairing offer admits a fixed number of attempts.
An idle attacker on the network can therefore exhaust those counts until the owner stops `serve`, which is a loss of availability, never of confidentiality or integrity.

Control of a running job goes through a local socket in the owner-only state directory, so the kernel's file permissions decide who may stop or follow a job, and another local user cannot even occupy its connections.

When a supervisor dies without finishing, for example because it was killed, its job is reported as lost from the moment its lock is free.
On Unix the job's own processes may outlive it until they next write output; on Windows the Job Object ends them with the supervisor.

A paired machine allowed to observe may search a job's log with a regular expression it chooses.
The search runs on the serving machine with the `regex` crate, whose matching time grows linearly with the log, never exponentially with the pattern; patterns are limited to 1024 bytes and their compiled forms to one mebibyte, and a search returns at most a thousand matches with at most twenty lines of context each.
Every line a machine returns, in a digest or a search, has its terminal control sequences removed before it leaves.
