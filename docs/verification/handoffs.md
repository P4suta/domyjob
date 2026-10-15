# Scoped handoffs

Use a handoff to continue work under your own registered agent identity.
The sender names each permitted reader by full agent ID and binds the body to an exact project and scope.
An intended agent must appear in those grants.
Intended team metadata describes the team.
Later room membership doesn't grant access.

Create a handoff with `domyjob chat --as SENDER handoff create --project PROJECT --scope SCOPE --recipient RECIPIENT_JSON --reader READER_ID TEXT`.
Repeat `--reader` for each permitted full agent ID.
Use `{"kind":"agent","agent":"NAME@ORIGIN"}` or `{"kind":"team","team":"room:ORIGIN:NAME"}` as the recipient JSON.
Replace the uppercase placeholders with actual identities and values.
The body accepts at most 65,536 UTF-8 bytes.
Project and scope each admit at most 256 UTF-8 bytes.

Discover metadata without fetching bodies:

```sh
domyjob chat --json --as continuing handoff list --project owner/project --scope issue-47 --limit 8
```

Listing returns the acting reader and up to 32 descriptors from the separate metadata index.
Review the sender, intended recipient or team, project, scope, explicit readers, and body byte count.
Pass one exact descriptor from the `handoffs` array to the dedicated read operation:

```sh
domyjob chat --json --as continuing handoff read --descriptor DESCRIPTOR_JSON --max-bytes BYTE_LIMIT
```

Set `BYTE_LIMIT` to cover the selected UTF-8 body byte count, up to 65,536.
Changing a descriptor, using an ungranted identity or selecting an insufficient bound refuses the read before body access.
Ordinary inbox, thread, read, and dashboard operations hide handoff bodies.
Continue with your own identity without opening the previous recipient's private inbox.

CLI and MCP publish an actual-reader observation after writing and flushing the complete original body response.
Output errors, cancellation, and replacement protocol errors prevent observation.
Observations name the acting full identity and exact handoff.
Retries join idempotently.
Observations don't advance another agent's read status or prove that an AI understood the body.
A crash between output and observation can repeat the response.

The current store format preserves the prior notification and original chat formats as explicit upgrade inputs.
Normal store initialization adds indexes.
Passive dashboard browsing leaves the database format unchanged.
Older binaries must refuse the new format.
Experimental preserved-request-context stores use different format identities and are rejected rather than downgraded.

The handoff decision harnesses and named false controls declare scalar read, observation and response-delivery obligations.
Typed, store, CLI, and MCP fixtures cover metadata/body binding, exact readers, private export, output failure, and observation retry.
Actual verification results must identify the exact source and tool configuration.
Peer-origin authentication, local identity registration, database durability and stream delivery remain external trust boundaries.

## Verify the decision contracts

Run `mise x -- cargo xtask verify handoffs` to check the three existing scalar decision proofs, their three named counterexamples, and nineteen required covers.
The fixed command accepts no harness names, source paths, solver choices, or timeout overrides.
It validates the complete inventory and Parent source refinement before selecting the exact core harnesses, cover counts, and negative assertions.
The existing runner checks physical tools, normal exit codes, the sixty-second harness deadline, and complete reports.
It checks the source fingerprint after the attempted validation or proof, including failure.

The harnesses import the production read, observation, and response-delivery decisions.
Their scalar domains cover grant and descriptor match facts, project and scope match facts, byte bounds, issuer and audience match facts, Lamport clocks, and cancellation.
Native fixtures remain required for the adapters that derive these facts, body and metadata binding, I/O, identity, and persistence.
Full `cargo xtask verify` remains a separate required gate for the complete project inventory.
