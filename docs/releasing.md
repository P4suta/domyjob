# Releasing

A release is one merge of the release pull request; everything else is automatic.
Releases go to GitHub Releases only; nothing is published to crates.io.
The release-plz configuration uses `git_only` to detect both workspace packages' versions from the shared `vX.Y.Z` tag instead of the Cargo registry.
See [release-plz's git-only configuration](https://release-plz.dev/docs/config#the-git_only-field).
Before the first release, complete the verification run below with the signing credentials configured.

Linux distribution builds retain Ubuntu 24.04 as their compatibility baseline.
CI uses the latest available stable Ubuntu and macOS images, independently of the distribution build baseline.

## How a release happens

1. On every push to `main`, release-plz keeps one pull request open that bumps the workspace version and writes `CHANGELOG.md` from the Conventional Commits since the last tag.
2. Merging that pull request makes release-plz cut the tag `vX.Y.Z`.
   Only a release pull request cuts a tag, so no other merge spends a version.
3. The tag starts `.github/workflows/release.yml`:
   - `check` refuses a tag that does not name the workspace version.
   - `build` compiles `domyjob` for x86-64 and Arm Linux, x86-64 and Arm macOS, and x86-64 Windows.
     It signs the macOS binaries with the Developer ID certificate and the hardened runtime and waits for Apple to accept their notarization.
     `cargo xtask release sign-macos` owns the temporary credentials and keychain, verifies the selected identity, and restores the original keychain search list when it finishes.
     It signs the Windows binary with SSL.com eSigner and requires a valid Authenticode signature with signer and timestamp certificates.
     The eSigner action selects `signing_method: v2` to use its configured Java runtime and current trust store instead of CodeSignTool's bundled legacy JDK.
     Each binary is packed with the licenses, README, and icon assets into a `.tar.gz`.
   - `publish` requires five archives, generates and checks their SHA-256 sums on Ubuntu, records GitHub's build provenance attestation for every archive, and publishes the release.

The workflow uses [GitHub CLI's draft-and-upload sequence](https://cli.github.com/manual/gh_release_create) to upload all assets before publication.
GitHub Release immutability takes effect when the release is published.
See [GitHub's immutable release documentation](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases).
The repository also protects `v*` tags against updates and deletion, so merging the release pull request and creating its tag belong to the final publication step.
The manual verification run below creates neither a tag nor a GitHub Release.

The Windows executable embeds the icon as a native resource during compilation, before signing.
The resource compiler must succeed for every normal Windows build.
The SVG retains the vector artwork; PNG, ICO, and ICNS copies are included in each archive's `assets` directory.
The icon uses Carbon's `intent-request-create` under Apache-2.0; its license and notice accompany the assets.
The macOS distribution remains a standalone command-line executable.
Its ICNS asset does not set a Finder or Dock icon; macOS application icons require an [application bundle](https://developer.apple.com/library/archive/documentation/CoreFoundation/Conceptual/CFBundles/BundleTypes/BundleTypes.html).

## What the signatures prove

| Mechanism | Purpose |
| --- | --- |
| Git commit signature | Identifies the signer of a source commit |
| Apple Developer ID and Windows Authenticode signatures | Identify the binary's publisher and detect changes after signing |
| Apple notarization | Records Apple's automated checks of the submitted macOS binary |
| GitHub build provenance attestation | Connects an archive to its source commit and the GitHub Actions workflow that built it |

A SHA-256 sum detects changes to an archive when compared with a trusted published sum.
It does not identify the publisher by itself.
Git commit signing uses the developer's existing Git signing configuration; it does not require either release certificate.

## One-time setup

- Install the release-plz GitHub App on this repository.
  Set its client ID as the repository variable `RELEASE_PLZ_APP_CLIENT_ID`, and its private key as the secret `RELEASE_PLZ_APP_PRIVATE_KEY` of the `release-plz` environment.
  Until the variable exists, the release-plz workflow skips its jobs.
  The client ID must be a repository variable because the job's `if` is evaluated before environment variables become available on a runner.
  See [GitHub's variable precedence documentation](https://docs.github.com/en/actions/reference/workflows-and-actions/variables#configuration-variable-precedence).
  A tag pushed with the App's token starts the release workflow, which a tag pushed with the workflow's own token would not.
- Set these secrets of the `release` environment:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | The Developer ID Application certificate and its private key, exported as `.p12` and encoded with base64 |
| `APPLE_CERTIFICATE_PASSWORD` | The password of that `.p12` |
| `APPLE_SIGNING_IDENTITY` | The 40-character SHA-1 fingerprint of the intended Developer ID Application signing identity |
| `APPLE_NOTARY_KEY` | The complete contents of an App Store Connect team API key (`.p8`) allowed to notarize |
| `APPLE_NOTARY_KEY_ID` | That key's ID |
| `APPLE_NOTARY_ISSUER_ID` | The issuer ID shown for the App Store Connect team API keys |
| `SSLDOTCOM_USERNAME` | The eSigner account's user name |
| `SSLDOTCOM_PASSWORD` | Its password |
| `SSLDOTCOM_CREDENTIAL_ID` | The eSigner credential ID of the code signing certificate |
| `SSLDOTCOM_TOTP_SECRET` | The persistent TOTP secret associated with that eSigner certificate |

### Apple signing certificate

Use the existing Apple Developer Program membership.
The certificate needed for this binary is **Developer ID Application**.
Apple's [Developer ID certificate guide](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/) explains the Account Holder requirement and the certificate types.

1. On the Mac that will keep the private key, open Keychain Access and choose Certificate Assistant > Request a Certificate from a Certificate Authority.
   Enter the user email address and a descriptive Common Name, leave the CA email address blank, and select Saved to disk.
   This creates the CSR and retains its private key in that Mac's keychain.
   Follow [Apple's CSR instructions](https://developer.apple.com/help/account/certificates/create-a-certificate-signing-request/).
2. In the Apple Developer account's Certificates, Identifiers & Profiles, create a Developer ID Application certificate with that CSR and download the `.cer` file.
3. Import the `.cer` on the same Mac.
   In Keychain Access > My Certificates, expand the certificate and confirm that its private key appears below it.
4. Select the certificate and its private key, choose File > Export Items, and export a password-protected `.p12`.
   Save the `.p12` and password in 1Password.
   A `.cer` alone contains no private key and cannot sign a binary.
   See [Apple's keychain export instructions](https://support.apple.com/guide/keychain-access/kyca35961/mac).
5. Run `security find-identity -v -p codesigning` and copy the 40-character SHA-1 fingerprint from the intended Developer ID Application row into `APPLE_SIGNING_IDENTITY`.
   The fingerprint selects the certificate without depending on its display name.

Validate the CI `.p12` with the workflow's native importer and include any intermediate certificate needed to build its trust chain.
The modern PKCS12 memory importer and `security import`'s automatic format detection can accept different formats.
If a generated CI export needs a compatible format, preserve the original export in 1Password and use the [documented PKCS12 compatibility options](https://cryptography.io/en/stable/hazmat/primitives/asymmetric/serialization/#pkcs12) for the derived input.

### Apple notarization key

The notarization key authenticates submissions to Apple; it is separate from the Developer ID certificate used to sign the binary.
The workflow passes `--key`, `--key-id`, and `--issuer` to `notarytool`, so configure a **team API key** with its issuer ID.
Follow [Apple's team API key instructions](https://developer.apple.com/help/app-store-connect/get-started/app-store-connect-api/) and [API key documentation](https://developer.apple.com/documentation/appstoreconnectapi/creating-api-keys-for-app-store-connect-api).

1. As the Account Holder, request API access in App Store Connect > Users and Access > Integrations if access is not already enabled.
2. As the Account Holder or an Admin, open Team Keys and generate a key for release notarization with the Developer role.
3. Download the private `.p8` key and save it in 1Password together with its Key ID and the team's Issuer ID.
   The private key can only be downloaded once.
4. Map the complete `.p8` contents, Key ID, and Issuer ID to the three `APPLE_NOTARY_*` secrets above.

### Windows eSigner credentials

Use the existing SSL.com certificate order and eSigner subscription.
Complete any outstanding identity validation and certificate issuance shown for that order.
Follow [SSL.com's enrollment guide](https://www.ssl.com/how-to/enroll-esigner-remote-document-ev-code-signing/) for OTP APP enrollment and save the enrollment PIN separately in 1Password.
The private signing key remains in SSL.com's cloud hardware security module; this workflow does not import a Windows `.p12`.
See [SSL.com's eSigner overview](https://www.ssl.com/esigner/).

Save the certificate's Credential ID together with the associated account username and password.
The Credential ID can be obtained with CodeSignTool's `get_credential_ids` command; [the signing action's documentation](https://github.com/SSLcom/esigner-codesign) describes this command.
If more than one certificate is listed, use `credential_info` to identify the code signing certificate for this release.

At the certificate's QR-code enrollment screen, save the persistent `secret code` in 1Password before leaving the page.
That value is `SSLDOTCOM_TOTP_SECRET`; it is neither a changing six-digit OTP nor the enrollment PIN.
The action uses this secret to generate each signing OTP automatically.
If enrollment is already complete and the secret was not saved, follow the QR-code recovery link in the enrollment guide before configuring automation.
See [SSL.com's automated signing instructions](https://www.ssl.com/how-to/automate-esigner-ev-code-signing/).

### Registering credentials

Keep private keys, passwords, and the TOTP secret in 1Password.
Use `gh secret set` with file input or its hidden interactive prompt, so their values do not appear in command arguments or shell history.
Do not paste them into issues, pull requests, or chat.
See [the GitHub CLI secret command](https://cli.github.com/manual/gh_secret_set).

Validate the credential files before registration.
The certificate example prepares and checks a nonempty private input before writing the remote secret, then removes it when the subshell exits.
On the Mac, replace only the file paths in these examples:

```sh
(
  set -euo pipefail
  umask 077
  certificate_input="$(mktemp "${TMPDIR:-/tmp}/domyjob-certificate.XXXXXX")"
  trap 'rm -f "$certificate_input"' EXIT
  test -s /path/to/developer-id.p12
  base64 -i /path/to/developer-id.p12 > "$certificate_input"
  test -s "$certificate_input"
  gh secret set APPLE_CERTIFICATE --repo P4suta/domyjob --env release < "$certificate_input"
)
gh secret set APPLE_NOTARY_KEY --repo P4suta/domyjob --env release < /path/to/AuthKey_KEYID.p8
gh secret set APPLE_CERTIFICATE_PASSWORD --repo P4suta/domyjob --env release
gh secret set APPLE_SIGNING_IDENTITY --repo P4suta/domyjob --env release
gh secret set APPLE_NOTARY_KEY_ID --repo P4suta/domyjob --env release
gh secret set APPLE_NOTARY_ISSUER_ID --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_USERNAME --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_PASSWORD --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_CREDENTIAL_ID --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_TOTP_SECRET --repo P4suta/domyjob --env release
```

The last eight commands prompt for each value.
For the release-plz App, use the repository variable and the separate environment secret:

```sh
gh variable set RELEASE_PLZ_APP_CLIENT_ID --repo P4suta/domyjob
gh secret set RELEASE_PLZ_APP_PRIVATE_KEY --repo P4suta/domyjob --env release-plz < /path/to/release-plz-app.pem
```

`gh secret list --repo P4suta/domyjob --env release` confirms the ten secret names without revealing their values.

## Verifying without publishing

After the workflow changes are on `main` and the credentials are configured, dispatch the release workflow on `main`:

```sh
gh workflow run release.yml --repo P4suta/domyjob --ref main
gh run list --repo P4suta/domyjob --workflow release.yml --event workflow_dispatch --limit 5
gh run watch RUN_ID --repo P4suta/domyjob --exit-status
gh run download RUN_ID --repo P4suta/domyjob --name dist-verified --dir dist-verified
```

Use the run ID returned by `gh run list` for the dispatched commit.
Manual execution reads the workspace version from that commit and runs the same five builds, signatures, notarization, packaging, checksums, and attestations as a tag push.
It saves five archives and five checksum files in `dist-verified` without creating a tag or GitHub Release.
Manual signing and notarization use the production services and consume the signing service's normal allowance.

Require all five build jobs and the final verification job to succeed.
Check every archive with its sum and GitHub attestation, then extract and run `domyjob --help` on native Linux, macOS, and Windows machines.
Complete the signature checks below for both macOS architectures and Windows before merging the first release pull request.

## Checking a download

- `gh attestation verify ARCHIVE --repo P4suta/domyjob --signer-workflow P4suta/domyjob/.github/workflows/release.yml` checks the archive's provenance and its release workflow identity.
  Check the displayed source commit against the tag or the manual verification run being reviewed.
  See [GitHub's attestation verification command](https://cli.github.com/manual/gh_attestation_verify).
- `shasum --algorithm 256 --check ARCHIVE.sha256` checks the archive against its published sum.
  On Linux, use `sha256sum --check ARCHIVE.sha256` instead.
- On macOS, `codesign --verify --strict --verbose=2 domyjob` checks the signature.
  `codesign --display --verbose=2 domyjob` shows the signing authority and hardened runtime, and `codesign --verify --verbose=2 -R='notarized' domyjob` checks notarization with an online connection.
  This distribution contains a standalone CLI binary; [Apple does not support stapling a notarization ticket to that format](https://developer.apple.com/videos/play/wwdc2019/703/?time=1948).
  Plan for the first Gatekeeper notarization check to require an online connection.
- On Windows, `Get-AuthenticodeSignature .\domyjob.exe | Format-List Status, SignerCertificate, TimeStamperCertificate` must report `Valid` with both certificates present.
  Compare the signer with the expected publisher.
