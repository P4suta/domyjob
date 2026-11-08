# Security model

OpenSSH authenticates the host and account.
A job runs with that account's authority and can execute arbitrary commands.
Trust source snapshots, job commands, and the remote build environment only within the account owner's authority.
The build fingerprint identifies deployed source.
It doesn't authenticate a publisher or dependency.

The storage adapter requires private ownership and Unix permissions or Windows ACLs.
Snapshot extraction uses confined directory capabilities and rejects links, nonregular files, nonportable paths, and collisions.
State records, control messages, and retained job output have explicit byte limits.
The output adapter neutralizes remote text before terminal display.

Jobs receive an explicit environment allowlist.
The launcher excludes SSH authentication and connection variables.
On macOS, a private Unix socket hands the worker to the desktop login session for native keychain access.
A missing session or login service causes refusal without an SSH fallback.

`domyjob` no longer supplies agent chat, managed agents, synchronization, MCP tools, or a dashboard.
`domyjob service install` and `uninstall` remove recognized domyjob client registrations and lifecycle handlers.
The migration keeps foreign entries, shared hook handlers, comments outside edited members, and chat history.
Malformed configuration stops migration without replacing the affected file.
Migration leaves chat databases unopened and preserves their files.
