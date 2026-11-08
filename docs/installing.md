# Installing

[Build from source](../README.md#installation) until the first GitHub Release.
Remote accounts need OpenSSH, mise, Rust, and Cargo.

## Login service

On macOS, run `domyjob service install` in the desktop login session before accepting jobs.
Keep the user logged in so workers can access the login keychain.
`domyjob service status` inspects the installed build and running service without changing configuration.
`domyjob service uninstall` stops the login service.
Running jobs keep independent process ownership.

The service commands migrate the old chat installation.
On macOS, installation keeps the `dev.domyjob.chat` launchd identity and changes its arguments to `service serve`.
On Linux and Windows, installation removes a recognized old chat service because persistent jobs launch directly.
Installation and removal delete recognized domyjob MCP registrations and lifecycle handlers from Codex, Claude Code, and OpenCode user configuration.
The migration keeps shared hooks, unrelated configuration, and historical chat files.
The command refuses an unrecognized service definition or malformed configuration.
Repair the named configuration and rerun the command after a failed migration.

`domyjob` no longer supplies agent chat, MCP tools, managed agents, synchronization, or a dashboard.
Use each client's own coordination features.
The [retirement decision](adr/retire-agent-chat-and-retain-job-service.md) records the boundary.

## macOS package

The macOS release uses a signed and notarized `.pkg` installer.
After publication, download the package for the Mac's architecture from [GitHub Releases](https://github.com/P4suta/domyjob/releases).
Apple Silicon uses `domyjob-VERSION-aarch64-apple-darwin.pkg`.
Intel uses `domyjob-VERSION-x86_64-apple-darwin.pkg`.
Apple menu > About This Mac identifies the processor.

Follow the [download verification instructions](releasing.md#checking-a-download) before installation.
Open the package and follow the Installer prompts, including administrator authorization.
Open a new Terminal window and run `domyjob --help`.
The installer writes the executable to `/Library/domyjob/bin` and adds that directory through `/etc/paths.d/domyjob`.
It leaves shell configuration files unchanged.
Use `/Library/domyjob/bin/domyjob --help` directly when the shell ignores the system PATH configuration.

The release pipeline signs the executable with Developer ID Application and the package with Developer ID Installer.
A stapled Apple notarization ticket supports an offline notarization check.
See [Apple's notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

Install the later package for the same architecture to upgrade.
The package keeps the identifier `io.github.p4suta.domyjob` and replaces its own files.
Jobs, configuration, and historical chat data stay in separate user storage.
Use `type -a domyjob` to inspect command selection when more than one installation exists.

## Package removal

Run `domyjob service uninstall` before removing the installed program.
Inspect the package receipt, executable publisher, and PATH entry:

```sh
pkgutil --pkg-info io.github.p4suta.domyjob
pkgutil --files io.github.p4suta.domyjob
codesign --verify --strict /Library/domyjob/bin/domyjob
codesign --display --verbose=2 /Library/domyjob/bin/domyjob
cat /etc/paths.d/domyjob
```

Continue only when the receipt identifies the intended installation, the publisher matches, and the PATH file contains only `/Library/domyjob/bin`.
Preserve replaced or customized files.
Remove the package's exact files and forget its receipt:

```sh
sudo rm /Library/domyjob/bin/domyjob \
  /Library/domyjob/share/README.md \
  /Library/domyjob/share/LICENSE-MIT \
  /Library/domyjob/share/LICENSE-APACHE \
  /etc/paths.d/domyjob
sudo rmdir /Library/domyjob/bin /Library/domyjob/share /Library/domyjob
sudo pkgutil --forget io.github.p4suta.domyjob
```

`rmdir` removes only empty directories.
`pkgutil --forget` removes the receipt without deleting installed files.
Open a new Terminal window afterward.
The commands preserve user state under `~/.local/state/domyjob` or the location selected by `DOMYJOB_STATE` or `XDG_STATE_HOME`.

## Linux and Windows archives

The release uses `.tar.gz` archives for Linux and Windows.
Linux x86-64 uses `domyjob-VERSION-x86_64-unknown-linux-gnu.tar.gz`.
Linux Arm64 uses `domyjob-VERSION-aarch64-unknown-linux-gnu.tar.gz`.
Windows x86-64 uses `domyjob-VERSION-x86_64-pc-windows-msvc.tar.gz`.

Each archive contains the executable, licenses, README, and icon assets.
The release pipeline signs `domyjob.exe` with SSL.com eSigner and a trusted timestamp.
Follow the [download verification instructions](releasing.md#checking-a-download), unpack the archive, and put the executable in a directory on PATH.
