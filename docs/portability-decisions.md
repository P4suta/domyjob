# Portability decisions

These decisions record why a platform-specific implementation remains after the common control flow has been unified.
The branch-count gate in `xtask` prevents new operating-system branches and requires its allowance to fall whenever a branch disappears.

## Secret storage

domyjob keeps the existing native macOS Keychain and Windows Data Protection API adapters instead of replacing them with the `keyring` crate.
Linux must continue to work on headless machines without a desktop secret service, where the owner-only state file is the intended storage.
Using `keyring` would therefore retain a Linux fallback and would move the two native branches behind another abstraction without producing one reliable path on every supported machine.

## Process lifetime

domyjob keeps its process implementation instead of adopting `process-wrap`.
That crate can represent Unix process groups and Windows Job Objects, but it does not replace domyjob's Unix reaper, interruptible output collection, or WMI-based Windows launch.
Adopting it would leave the difficult platform branches in place while adding a second lifetime abstraction around them.

## Linux login lifetime probe

The local `wip/linger` branch is retained as diagnostic history and is not part of the product.
It was built to test whether jobs died with the last Linux login, but the observed failure was a reboot and the affected machine already used `KillUserProcesses=no`.
The production design therefore relies on its service manager and crash recovery rather than a login-linger probe.
