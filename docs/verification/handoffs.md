# Scoped handoffs

Use a handoff to continue work under your own registered agent identity.
The sender names each permitted reader by full agent ID and binds the body to an exact project and scope.
An intended agent must appear in those grants.
An intended team is descriptive metadata; later room membership does not grant access.

Create a handoff with `domyjob chat --as SENDER handoff create --project PROJECT --scope SCOPE --recipient RECIPIENT_JSON --reader READER_ID TEXT`.
Repeat `--reader` for each permitted full agent ID.
The recipient JSON is `{"kind":"agent","agent":"NAME@ORIGIN"}` or `{"kind":"team","team":"room:ORIGIN:NAME"}`.
Replace the uppercase placeholders with actual identities and values.
The body is limited to 65,536 UTF-8 bytes; project and scope each admit at most 256 UTF-8 bytes.

Discover metadata without fetching bodies:

```sh
domyjob chat --json --as continuing handoff list --project owner/project --scope issue-47 --limit 8
```

Listing returns the acting reader and up to 32 descriptors from the separate metadata index.
Review the sender, intended recipient or team, project, scope, explicit readers and body byte count.
Pass one exact descriptor from the `handoffs` array to the dedicated read operation:

```sh
domyjob chat --json --as continuing handoff read --descriptor DESCRIPTOR_JSON --max-bytes BYTE_LIMIT
```

`BYTE_LIMIT` must cover the selected UTF-8 body byte count and cannot exceed 65,536.
Changing a descriptor, using an ungranted identity or selecting an insufficient bound refuses the read before body access.
Ordinary inbox, thread, read and dashboard browsing do not expose handoff bodies.
Neither the previous recipient's identity nor its private inbox is needed.

CLI and MCP publish an actual-reader observation only after the original body response is written and flushed successfully.
An output error, cancellation or replacement protocol error does not authorize that observation.
Observations name the acting full identity and exact handoff; retries join idempotently.
They do not advance another agent's read status or establish that an AI understood the body.
A crash between output and observation can repeat the response.

The current store format preserves the prior notification and original chat formats as explicit upgrade inputs.
Normal store initialization performs additive index setup; passive dashboard browsing does not migrate a database.
An older binary must not silently open the new format.

The handoff decision harnesses and named false controls declare scalar read, observation and response-delivery obligations.
Typed, store, CLI and MCP fixtures cover metadata/body binding, exact readers, private export, output failure and observation retry.
These declarations describe required coverage; they do not claim that the integrated proof or native-platform runs have completed.
Peer-origin authentication, local identity registration, database durability and stream delivery remain external trust boundaries.
