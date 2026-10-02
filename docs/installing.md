# Installing

Until the first GitHub Release is published, [build from source](../README.md#install).

## macOS

The macOS download is a signed, notarized `.pkg` installer.
Once a release is published, download the package matching your Mac from [GitHub Releases](https://github.com/P4suta/domyjob/releases):

| Mac | Package |
| --- | --- |
| Apple Silicon | `domyjob-VERSION-aarch64-apple-darwin.pkg` |
| Intel | `domyjob-VERSION-x86_64-apple-darwin.pkg` |

Choose Apple menu > About This Mac to check whether it lists an Apple chip or an Intel processor.
The [release verification instructions](releasing.md#checking-a-download) explain how to check its checksum, signatures, and authenticated source manifests before installation.
Open the package, follow the Installer prompts, then open a new Terminal window and run `domyjob --help`.
The installer requires administrator authorization.
It installs the executable in `/Library/domyjob/bin` and adds that directory to new login shells through `/etc/paths.d/domyjob`.
It does not modify your shell configuration files.
Run `/Library/domyjob/bin/domyjob --help` directly if your shell does not load the system PATH configuration.

The executable has a Developer ID Application signature, and the package has a separate Developer ID Installer signature.
The published package also carries a stapled Apple notarization ticket, allowing its notarization check without an online ticket lookup.
Stapling adds Apple's ticket to the installer; it does not turn the CLI into a graphical application.
See [Apple's notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

To upgrade, install the newer package for the same architecture.
The package identifier stays `io.github.p4suta.domyjob`, and the installer replaces the package's files.
Jobs, configuration, and chat data are stored separately and are preserved.
Use `type -a domyjob` to check which executable your shell selects if you also installed another version.

### Uninstalling the package

First inspect the package receipt, installed executable, and PATH entry:

```sh
pkgutil --pkg-info io.github.p4suta.domyjob
pkgutil --files io.github.p4suta.domyjob
codesign --verify --strict /Library/domyjob/bin/domyjob
codesign --display --verbose=2 /Library/domyjob/bin/domyjob
cat /etc/paths.d/domyjob
```

Continue only when the receipt belongs to this installer, the executable has the expected publisher, and the PATH file contains only `/Library/domyjob/bin`.
If you have replaced any installed file or customized the PATH file, preserve it and remove only the files that still belong to this installation.
The commands below remove the package's exact files and forget its receipt:

```sh
sudo rm /Library/domyjob/bin/domyjob \
  /Library/domyjob/share/README.md \
  /Library/domyjob/share/LICENSE-MIT \
  /Library/domyjob/share/LICENSE-APACHE \
  /etc/paths.d/domyjob
sudo rmdir /Library/domyjob/bin /Library/domyjob/share /Library/domyjob
sudo pkgutil --forget io.github.p4suta.domyjob
```

`rmdir` removes only empty directories, preserving any other files you placed there.
`pkgutil --forget` removes the receipt; it does not remove installed files by itself.
Open a new Terminal window afterward.
User state, normally `~/.local/state/domyjob` or the directory selected by `DOMYJOB_STATE` or `XDG_STATE_HOME`, is preserved.

## Linux and Windows

Linux and Windows downloads are `.tar.gz` archives:

| Platform | Archive |
| --- | --- |
| Linux x86-64 | `domyjob-VERSION-x86_64-unknown-linux-gnu.tar.gz` |
| Linux Arm64 | `domyjob-VERSION-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86-64 | `domyjob-VERSION-x86_64-pc-windows-msvc.tar.gz` |

They contain the executable, licenses, README, and icon assets without an installer.
The Windows archive contains `domyjob.exe` signed with SSL.com eSigner and a trusted timestamp.
Extract the archive matching your platform and place the executable in a directory on your PATH.
Use the [release verification instructions](releasing.md#checking-a-download) to check the download.
