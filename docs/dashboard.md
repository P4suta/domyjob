# Human dashboard

Run `domyjob tui`, or invoke `domyjob` without a command in an interactive terminal.
Scripted invocations need an explicit command; `--help` remains available without a terminal.
Use `domyjob tui --as NAME` to start with a registered local viewing identity.
The header identifies the viewing identity without advertising activity or presence.

Machines show the existing local ledger and peer synchronization observations.
Agents show the actual provider, profile and activity observation with its independent expiry.
Chats show conversations accessible to the explicitly selected local identity and the latest bounded message page with each declared recipient's current delivery stages.
Unknown clocks and clock rollback remain unknown, old synchronization and activity observations are marked expired, and failed synchronization is labelled offline independently of agent activity.
Refresh errors retain the previous observations with an explicit error label.

- `Tab` changes the Machines, Agents and Chats section.
- Arrow keys or `j` and `k` move the selection.
- `Enter` opens detail; `Esc` returns or clears search.
- `/` opens bounded Unicode search.
- `a` opens the local viewing-identity picker.
- `PageUp` and `PageDown` scroll detail.
- `r` refreshes local observations.
- `?` opens keyboard help; any key closes it.
- `q` or `Ctrl+C` exits the dashboard.

The dashboard adapts to 128 by 36, 80 by 24 and 48 by 12 terminals and provides an exit and resize prompt below the supported viewport bounds.
It uses ordinary terminal fonts with visible selection and text labels that remain meaningful without color.
Browsing does not mark messages read, publish activity, join identities, start the chat service, synchronize a peer, dispatch a job or consume a completed result.
An already running chat service can supply new peer observations while the dashboard reads local state.
Control characters and directional overrides are escaped before they reach the terminal.
Terminal setup is fallible, and every owned cleanup is attempted while preserving both primary and cleanup diagnostics.
