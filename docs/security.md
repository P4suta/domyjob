# Security model

OpenSSH authentication controls who may request work on a machine.
The remote node executes commands as the SSH account and has no separate network listener.
A user who can run `domyjob on` on a host can execute arbitrary commands as that account.

The node treats control frames, snapshots, stored records, and remote output as untrusted data.
Ingress validates bounded wire messages before dispatch.
A source snapshot is checked for size, digest, portable paths, case collisions, and regular file types before confined extraction.
State files are private and checked for owner and access permissions before use.
Log replies read at most the final 16 KiB of the output file and terminal text is neutralized.
The output file stores at most the first 256 MiB of a job's output, and one final line counts any later bytes that were discarded.
Jobs receive an explicit environment allowlist, so SSH agent and connection variables are not passed to the command.

The client builds and installs remote code from its embedded build source over an authenticated SSH connection.
Every build runs from its own fingerprint-named path, so a request always reaches the node of its own build.
The fingerprint detects changes; it does not certify the source or dependencies.
The remote account and build environment are trusted to compile and execute that checkout.

Chat runs within the same trust boundary.
A machine pins each peer's chat identity and refuses an exchange meant for another identity.
Any account that can reach a machine over SSH can read the chat events addressed to that machine's agents and can ask its managed agents to run turns with their configured access.
Managed agents default to read-only access, and each client's own sandbox or permission mode enforces it.
The MCP server writes only as an agent that joined or was bound to the connection, and it does not expose job operations.
Setup changes the user's AI client configuration only through each client's registration command or a comment-preserving edit of the domyjob entry.

The first release does not provide a network service, signed release installation, file pullback, or migration from the former state format.
