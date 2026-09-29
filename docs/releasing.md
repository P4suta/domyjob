# Releasing

A release is one merge of the release pull request; everything else is automatic.
Releases go to GitHub Releases only; nothing is published to crates.io.

## How a release happens

1. On every push to `main`, release-plz keeps one pull request open that bumps the workspace version and writes `CHANGELOG.md` from the Conventional Commits since the last tag.
2. Merging that pull request makes release-plz cut the tag `vX.Y.Z`.
   Only a release pull request cuts a tag, so no other merge spends a version.
3. The tag starts `.github/workflows/release.yml`:
   - `check` refuses a tag that does not name the workspace version.
   - `build` compiles `domyjob` for x86-64 and Arm Linux, x86-64 and Arm macOS, and x86-64 Windows.
     It signs the macOS binaries with the Developer ID certificate and the hardened runtime and has Apple notarize them,
     signs the Windows binary with SSL.com eSigner,
     and packs each binary with the licenses and README into a `.tar.gz` with its SHA-256 sum.
   - `publish` records GitHub's build provenance attestation for every archive and publishes the release.

## One-time setup

- Install the release-plz GitHub App on this repository.
  Set its client ID as the repository variable `RELEASE_PLZ_APP_CLIENT_ID`, and its private key as the secret `RELEASE_PLZ_APP_PRIVATE_KEY` of the `release-plz` environment.
  Until the variable exists, the release-plz workflow skips its jobs.
  A tag pushed with the App's token starts the release workflow, which a tag pushed with the workflow's own token would not.
- Set these secrets of the `release` environment:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | The Developer ID Application certificate and its private key, exported as `.p12` and encoded with base64 |
| `APPLE_CERTIFICATE_PASSWORD` | The password of that `.p12` |
| `APPLE_SIGNING_IDENTITY` | The certificate's name, such as `Developer ID Application: NAME (TEAMID)` |
| `APPLE_NOTARY_KEY` | The contents of an App Store Connect API key (`.p8`) allowed to notarize |
| `APPLE_NOTARY_KEY_ID` | That key's ID |
| `APPLE_NOTARY_ISSUER_ID` | The issuer ID of App Store Connect API keys |
| `SSLDOTCOM_USERNAME` | The eSigner account's user name |
| `SSLDOTCOM_PASSWORD` | Its password |
| `SSLDOTCOM_CREDENTIAL_ID` | The eSigner credential ID of the code signing certificate |
| `SSLDOTCOM_TOTP_SECRET` | The TOTP secret of eSigner's automated signing |

## Checking a download

- `gh attestation verify ARCHIVE --repo P4suta/domyjob` checks that an archive was built by this repository's release workflow from the tagged commit.
- `shasum --algorithm 256 --check ARCHIVE.sha256` checks the archive against its published sum.
- On macOS, `codesign --verify --strict --verbose=2 domyjob` checks the signature, and Gatekeeper checks the notarization online the first time a downloaded binary runs.
- On Windows, `Get-AuthenticodeSignature domyjob.exe` shows the signer.
