# Client lifecycle hooks

Managed Claude invocations receive synchronous SessionStart, PreToolUse, PostToolUse, and Stop hooks through inline `--settings`.
The executable and each argument use Claude's exec-form hook schema, so a program path remains one literal argument.
Existing user and project hooks remain in their own settings scopes.
The adapter requires Claude Code's documented exec-form command hooks with an explicit argument array.

`domyjob chat setup` merges the four corresponding Codex hooks into the user `hooks.json` next to the active user configuration, honoring `CODEX_HOME`.
It preserves unrelated matcher groups, unrelated events, and document metadata.
Only a single command handler bearing the generated `domyjob: chat lifecycle` status marker and both platform commands is owned by the updater.
Mixed groups retain all their handlers.
An invalid selected event or matcher-group shape aborts before writing the configuration.
Repeated setup with the same build leaves the document unchanged.
MCP setup reports success only after observing the exact registration, and a failed add or mismatched post-add query aborts before hook configuration.
Structured registration and credential queries require complete captured output; a truncated response cannot certify readiness.
The installed build path changes the hook definition when upgrading, so Codex may require review again.

Codex requires native trust of each exact hook definition.
Setup and doctor report `needs-review` and direct the owner to `/hooks`; they do not infer trust from a configuration file or write trusted hashes.
The launch adapter passes no hook-trust bypass option.
The status remains a review requirement because domyjob does not query Codex's native trust authority.
The owner can inspect the effective enabled and trusted state in Codex's hook browser.
Disabled, untrusted, unsupported, or failing client hooks do not establish notification delivery.

Generated persistent hooks use `--if-bound`, which returns an empty successful client response before opening the chat store when no domyjob actor is bound.
Managed invocations inherit their exact agent and turn identity.
POSIX commands quote the executable as a literal shell word.
The Windows override uses a literal PowerShell executable argument inside an encoded command; the enclosing command contains no expandable program-path text.

The source-bound Kani compaction obligation proves preservation and order within its stated finite domain.
Native fixtures and the preserved fuzz corpus verify the JSON classifier, replacement insertion, idempotence, malformed-input refusal, and client command construction.
The bounded Miri lane interprets the pure merge contracts.
These checks do not prove client hook scheduling, native trust, client consumption of emitted context, or all filesystem behavior.

The adapter follows the [Codex hook reference](https://learn.chatgpt.com/docs/hooks) and the [Claude hook reference](https://code.claude.com/docs/en/hooks).
