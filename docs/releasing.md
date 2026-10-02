# Releasing

A release begins with one merge of the release pull request; publication follows after Apple accepts both macOS submissions.
Releases go to GitHub Releases only; nothing is published to crates.io.
The release-plz configuration uses `git_only` to detect both workspace packages' versions from the shared `vX.Y.Z` tag instead of the Cargo registry.
See [release-plz's git-only configuration](https://release-plz.dev/docs/config#the-git_only-field).
Before the first release, complete the verification run below with the signing credentials configured.
Public releases contain two macOS `.pkg` installers, two Linux `.tar.gz` archives, and one Windows `.tar.gz` archive containing the signed executable.
The two macOS archives remain internal verification artifacts and are not published as downloads.
See [Installing](installing.md) for the package layout and removal instructions.

Linux distribution builds retain Ubuntu 24.04 as their compatibility baseline.
CI uses the latest available stable Ubuntu and macOS images, independently of the distribution build baseline.
Release tooling pins crc32fast to 1.5.0 because later versions' wide SIMD CRC injection uses casts whose upper lanes are not guaranteed to be zero; this dependency is confined to xtask.

## How a release happens

1. On every push to `main`, release-plz keeps one pull request open that bumps the workspace version and writes `CHANGELOG.md` from the Conventional Commits since the last tag.
2. Merging that pull request makes release-plz cut the tag `vX.Y.Z`.
   Only a release pull request cuts a tag, so no other merge spends a version.
3. The tag starts `.github/workflows/release.yml`:
   - `check` refuses a tag that does not name the workspace version and runs the pinned deny, audit, and vet tools before any build or signature.
     The audit refreshes the RustSec advisory database even when `Cargo.lock` has not changed, so a newly disclosed dependency advisory cannot bypass the release gate through scoped CI.
   - `build` compiles `domyjob` for x86-64 and Arm Linux, x86-64 and Arm macOS, and x86-64 Windows.
     It signs the macOS binaries with Developer ID Application and the hardened runtime, then creates and signs each installer with the separate Developer ID Installer identity.
     The installer contains `/Library/domyjob/bin/domyjob`, `/Library/domyjob/share/{README.md,LICENSE-MIT,LICENSE-APACHE}`, and `/etc/paths.d/domyjob`, whose content is exactly `/Library/domyjob/bin` followed by a newline.
     The stable package identifier is `io.github.p4suta.domyjob`; the package contains no installation scripts.
     `cargo xtask release sign-macos` owns the temporary credentials and keychain, verifies both selected identities, restores the original keychain search list, and submits the signed package to Apple without waiting.
     It saves a public submission receipt bound to the package and embedded executable before it finishes.
     It signs the Windows binary with SSL.com eSigner and requires a valid Authenticode signature with signer and timestamp certificates and the exact configured signing certificate SHA-256.
     The eSigner action selects `signing_method: v2` to use its configured Java runtime and current trust store instead of CodeSignTool's bundled legacy JDK.
     Each binary is packed with the licenses, README, and icon assets into a `.tar.gz`.
     The macOS archives preserve the signed executable for internal verification rather than public distribution.
   - `handoff` requires five archives, two signed installers, and two submission receipts, generates and checks all seven asset SHA-256 sums, and records GitHub's build provenance attestation for every original asset and the source manifest in the original build run.
     It preserves `dist-pending` for 14 days and finishes without publishing a release.
4. `.github/workflows/release-finalize.yml` checks existing submissions after a completed build, once an hour, or on an explicit manual request.
   Ubuntu discovery scans at most the latest 100 build runs; macOS jobs start only for eligible pending runs.
   `cargo xtask release queue finalize` verifies the original run, attempt, source, artifacts, checksums, and provenance before querying each Apple submission once.
   It requires the original source commit to remain an ancestor of current `main` and checks this again before publication.
   A pending result finishes successfully without rebuilding, signing, submitting, or publishing anything.
   After both submissions are accepted, the task checks the saved signatures and notarization, then copies the pending distribution into a separate `ready/` directory.
   It staples and validates Apple's tickets on those installer copies, rechecks their signatures and payloads, and generates the final checksums without modifying the original pending artifacts.
   `release-derivation.json` records the original source, run, attempt, and pending manifest digest, the finalizer's source, run, and attempt, and the original and final asset digests.
   `build-manifest.json` is an unchanged copy of the original pending manifest and retains the original build attestation because its digest is unchanged.
   The finalizer attests the stapled installers and derivation receipt and preserves `dist-ready-ORIGINAL_RUN_ID-ORIGINAL_ATTEMPT` before publication.
   A tag origin may publish only when its version, original commit, and current protected tag agree.
   Before every publication attempt, it retrieves `Cargo.lock` from that original exact source commit and audits those bytes against a freshly fetched RustSec advisory database.
   The finalizer's current `main` lockfile cannot substitute for the dependencies in the waiting release.
   A source lookup, database fetch, audit, or lockfile digest mismatch stops the attempt before any release write.
   Publication uploads exactly twelve files: the five public assets, their five checksum files, `build-manifest.json`, and `release-derivation.json`.
   The two internal macOS archives are excluded.
   A manual build origin never publishes, even when a later automatic finalizer accepts it.
   A successful completion preserves `dist-verified-ORIGINAL_RUN_ID-ORIGINAL_ATTEMPT`; manual rehearsals preserve both artifacts without publishing.

Archived schema-1 rehearsals remain resumable as their original five-archive distributions with ZIP-based notarization receipts.
They do not gain installers or require an additional submission to resume, and their manual build origin still cannot publish a release.
New schema-2 handoffs bind the two signed installers and their embedded executable identities to the source manifest and Apple submission receipts.

The workflow uses [GitHub CLI's draft-and-upload sequence](https://cli.github.com/manual/gh_release_create) to upload all assets before publication.
The finalizer executes from the repository's trusted `main`; source ancestry verifies membership in that history rather than independently auditing branch protection settings.
GitHub Release immutability takes effect when the release is published.
See [GitHub's immutable release documentation](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases).
The repository also protects `v*` tags against updates and deletion, so merging the release pull request and creating its tag belong to the final publication step.
Before signing and publication, the same tag policy check requires an authenticated [GraphQL ruleset bypass connection](https://docs.github.com/en/graphql/reference/repos#repositoryruleset) with an explicit zero `totalCount` and consistent empty page; this trusts GitHub's connection contract and never treats omitted REST bypass metadata as an empty list.
The manual verification run below creates neither a tag nor a GitHub Release.

Notarization has a seven-day queue deadline, independent of the 15-minute finalizer job limit.
Expired entries appear in scheduled summaries, and an explicit request for an expired entry fails.
This does not cancel Apple's processing or request another submission.
Keep the original archives, signed packages, and receipts; a new signature or build changes the bytes being verified.
Stapling changes a package's digest, so ready installers have finalizer provenance and final checksums in addition to the original build provenance.
Publishing the authenticated build manifest and derivation receipt preserves the source lineage after the temporary Actions artifacts expire.
Apple supports checking a saved submission ID independently of the upload, and its service continues processing after a client wait times out.
See [Apple's notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

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
| Apple Developer ID and Windows Authenticode signatures | Identify the binary or installer's publisher and detect changes after signing |
| Apple notarization | Records Apple's automated checks of the submitted macOS installer and its signed executable |
| GitHub build provenance attestation | Connects an original asset to its source commit and the GitHub Actions workflow that built it |
| Finalizer attestation and derivation receipt | Connect the stapled installer to its original signed package and the finalizer that added the ticket |
| GitHub immutable release attestation | Records the published release tag, commit, and exact attached asset digests |

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
| `APPLE_INSTALLER_CERTIFICATE` | The separate Developer ID Installer certificate and its private key, exported as `.p12` and encoded with base64 |
| `APPLE_INSTALLER_CERTIFICATE_PASSWORD` | The password of that installer `.p12` |
| `APPLE_INSTALLER_IDENTITY` | The 40-character SHA-1 fingerprint of the intended Developer ID Installer signing identity |
| `APPLE_NOTARY_KEY` | The complete contents of an App Store Connect team API key (`.p8`) allowed to notarize |
| `APPLE_NOTARY_KEY_ID` | That key's ID |
| `APPLE_NOTARY_ISSUER_ID` | The issuer ID shown for the App Store Connect team API keys |
| `SSLDOTCOM_USERNAME` | The eSigner account's user name |
| `SSLDOTCOM_PASSWORD` | Its password |
| `SSLDOTCOM_CREDENTIAL_ID` | The eSigner credential ID of the code signing certificate |
| `SSLDOTCOM_TOTP_SECRET` | The persistent TOTP secret associated with that eSigner certificate |

### Apple signing certificate

Use the existing Apple Developer Program membership.
The binary requires **Developer ID Application**, and its `.pkg` requires a separate **Developer ID Installer** certificate from the same team.
Apple Distribution and Mac App Store installer certificates cannot replace the Developer ID Installer certificate.
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

Repeat the certificate creation and password-protected export for Developer ID Installer, keeping both certificate backups in 1Password.
Use `security find-identity -v` to list installer identities, because the `codesigning` policy above selects application identities.
Map the installer export, its password, and its fingerprint to the three `APPLE_INSTALLER_*` secrets.
An existing application certificate and notarization API key do not supply the installer identity; complete that setup before running the new package rehearsal.

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

Set the non-secret `WINDOWS_SIGNING_IDENTITY_SHA256` variable in the `release` environment to the SHA-256 of the intended signing certificate's DER bytes (`SignerCertificate.RawData`).
Use exactly 64 lowercase hexadecimal characters; this is neither the executable's SHA-256 nor the usual Windows SHA-1 certificate thumbprint.
Verification requires that exact leaf certificate in addition to a valid Authenticode signature and timestamp, so another trusted publisher cannot satisfy the release gate.
When the certificate is renewed, verify the replacement publisher and update the variable to its new certificate digest.

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
gh secret set APPLE_INSTALLER_CERTIFICATE_PASSWORD --repo P4suta/domyjob --env release
gh secret set APPLE_INSTALLER_IDENTITY --repo P4suta/domyjob --env release
gh secret set APPLE_NOTARY_KEY_ID --repo P4suta/domyjob --env release
gh secret set APPLE_NOTARY_ISSUER_ID --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_USERNAME --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_PASSWORD --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_CREDENTIAL_ID --repo P4suta/domyjob --env release
gh secret set SSLDOTCOM_TOTP_SECRET --repo P4suta/domyjob --env release
```

Repeat the validated base64 file-input example for the installer `.p12`, setting `APPLE_INSTALLER_CERTIFICATE` instead of `APPLE_CERTIFICATE`.
The password and fingerprint commands use hidden prompts.
For the release-plz App, use the repository variable and the separate environment secret:

```sh
gh variable set RELEASE_PLZ_APP_CLIENT_ID --repo P4suta/domyjob
gh secret set RELEASE_PLZ_APP_PRIVATE_KEY --repo P4suta/domyjob --env release-plz < /path/to/release-plz-app.pem
```

`gh secret list --repo P4suta/domyjob --env release` confirms the thirteen secret names without revealing their values.

## Verifying without publishing

After the workflow changes are on `main` and the credentials are configured, dispatch the release workflow on `main`:

```sh
gh workflow run release.yml --repo P4suta/domyjob --ref main
gh run list --repo P4suta/domyjob --workflow release.yml --event workflow_dispatch --limit 5
gh run watch RUN_ID --repo P4suta/domyjob --exit-status
gh run download RUN_ID --repo P4suta/domyjob --name dist-pending --dir dist-pending
gh run list --repo P4suta/domyjob --workflow release-finalize.yml --limit 5
```

Use the run ID returned by `gh run list` for the dispatched commit.
Manual execution is restricted to `main`, reads the workspace version from that commit, and runs the same five builds, signatures, submissions, packaging, checksums, and attestations as a tag push.
It saves five archives, two signed installers, seven checksum files, two submission receipts, and a source manifest in `dist-pending` without creating a tag or GitHub Release.
Both pending and ready distributions retain all seven internal assets and checksums, including the macOS archives used to verify the signed executable against its submission receipt.
The ready distribution has sixteen files: seven assets, seven checksum files, `build-manifest.json`, and `release-derivation.json`.
Manual signing and notarization use the production services and consume the signing service's normal allowance.

The finalizer runs automatically after the build and checks again hourly while the queue remains eligible.
To check the same original submissions immediately without rebuilding or using another signing allowance, dispatch the finalizer on `main`:

```sh
gh workflow run release-finalize.yml --repo P4suta/domyjob --ref main -f run_id=ORIGINAL_RUN_ID
gh run list --repo P4suta/domyjob --workflow release-finalize.yml --limit 5
gh run watch FINALIZER_RUN_ID --repo P4suta/domyjob --exit-status
gh run download FINALIZER_RUN_ID --repo P4suta/domyjob --name dist-ready-ORIGINAL_RUN_ID-ORIGINAL_ATTEMPT --dir dist-ready
gh run download FINALIZER_RUN_ID --repo P4suta/domyjob --name dist-verified-ORIGINAL_RUN_ID-ORIGINAL_ATTEMPT --dir dist-verified
```

Read the original attempt from `dist-pending/manifest.json`.
A successful pending check has neither a ready distribution nor a `dist-verified` artifact and is not proof that notarization finished.
The accepted artifact becomes a completion marker only after its finalizer run succeeds, so a failed publication can be retried with the same original bytes.
Retries verify an existing release's asset digests and add only missing draft assets; they never replace published assets or move the protected tag.
The finalizer preserves the original build attestations and adds separate attestations for the stapled packages and derivation receipt.
The original binary source remains the build commit, even when a later trusted `main` commit performs finalization.

Require all five build jobs, the handoff job, and an accepted finalizer with its verified artifact to succeed.
Check every original asset with its sum and build attestation, and every ready installer with its final sum, finalizer attestation, and derivation receipt.
Extract and run `domyjob --help` on native Linux, macOS, and Windows machines, and validate both installers' exact payloads, signatures, and stapled tickets.
Complete the checks below before merging the first release pull request.

## Checking a download

Download the selected asset, its `.sha256` file, `build-manifest.json`, and `release-derivation.json` from the same release.
The build manifest identifies the original binary source; the derivation receipt separately identifies the finalizer that stapled the packages.
The original source must match the protected release tag, while the finalizer may use a later commit on `main`.

For a published immutable release, set `RELEASE_TAG` to the selected `vVERSION` and verify GitHub's release attestation and the downloaded asset:

```sh
gh release verify "$RELEASE_TAG" --repo P4suta/domyjob --format json
gh release verify-asset "$RELEASE_TAG" ASSET --repo P4suta/domyjob --format json
```

Repeat `verify-asset` for the checksum file and both manifests, and check that the attested tag and commit are the intended release.
GitHub [automatically creates this attestation when an immutable release is published](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases).
These [release](https://cli.github.com/manual/gh_release_verify) and [asset](https://cli.github.com/manual/gh_release_verify-asset) checks bind downloads to the published release; the original build and finalizer provenance below identify how those bytes were produced.
Manual rehearsals have no published release attestation.

Set `ORIGINAL_REF` to the selected `refs/tags/vVERSION` and `ORIGINAL_SHA` to that tag's commit, resolving an annotated tag to its commit if necessary.
For a manual rehearsal, use `refs/heads/main` and the original run's reviewed commit instead.
Set `FINALIZER_SHA` to the derivation receipt's claimed `producer.sourceSha`; that claim becomes authenticated only after its verification succeeds.
Verify both manifests before relying on their other fields:

```sh
gh attestation verify build-manifest.json --repo P4suta/domyjob \
  --signer-workflow P4suta/domyjob/.github/workflows/release.yml \
  --source-ref "$ORIGINAL_REF" --source-digest "$ORIGINAL_SHA" \
  --signer-digest "$ORIGINAL_SHA" --deny-self-hosted-runners \
  --predicate-type https://slsa.dev/provenance/v1 --digest-alg sha256 --format json
gh attestation verify release-derivation.json --repo P4suta/domyjob \
  --signer-workflow P4suta/domyjob/.github/workflows/release-finalize.yml \
  --source-ref refs/heads/main --source-digest "$FINALIZER_SHA" \
  --signer-digest "$FINALIZER_SHA" --deny-self-hosted-runners \
  --predicate-type https://slsa.dev/provenance/v1 --digest-alg sha256 --format json
```

Repeat the first command for the downloaded Linux or Windows archive, and the second command for a downloaded `.pkg`, replacing only the file argument.
See [GitHub's attestation verification command](https://cli.github.com/manual/gh_attestation_verify).
Compare each verified attestation's `runInvocationURI` with the corresponding original or producer run ID and attempt in the authenticated manifests.
The derivation receipt's `source` must equal the build manifest's `source`, and its `pendingManifestSha256` must equal the SHA-256 of `build-manifest.json`.
Its producer commit must belong to `main` and include the original source in its history.
For the selected asset, match its filename and `originalSha256` to the original manifest entry, then compare the downloaded bytes with the derivation's final `sha256` and the published checksum.
Archives keep their original digest; stapled packages have a separate final digest, so a final `.pkg` must not be checked against its original digest.
Dependency review and the repository's deny, audit, and vet checks remain separate from provenance verification.

- `shasum --algorithm 256 --check ASSET.sha256` checks the selected asset against its published sum.
  On Linux, use `sha256sum --check ASSET.sha256` instead.
  On Windows, compare `Get-FileHash .\ASSET -Algorithm SHA256` with the hash in `ASSET.sha256`.
- For a macOS installer, `pkgutil --check-signature PACKAGE.pkg` checks its Developer ID Installer signature, `xcrun stapler validate PACKAGE.pkg` checks its attached ticket, and `spctl --assess --type install --verbose=2 PACKAGE.pkg` checks Gatekeeper acceptance.
  Verify the expected publisher, architecture, and exact payload paths before installing.
- For the macOS executable, `codesign --verify --strict --verbose=2 domyjob` checks the signature.
  `codesign --display --verbose=2 domyjob` shows the signing authority and hardened runtime, and `codesign --verify --verbose=2 -R='notarized' --check-notarization domyjob` forces an online notarization check.
  [Apple does not support stapling a notarization ticket to a standalone CLI binary](https://developer.apple.com/videos/play/wwdc2019/703/?time=1948).
  The published installer carries the ticket instead; the standalone executable in the internal verification archive requires an online notarization check.
- On Windows, `Get-AuthenticodeSignature .\domyjob.exe | Format-List Status, SignerCertificate, TimeStamperCertificate` must report `Valid` with both certificates present.
  Compare the signer with the expected publisher and its DER certificate SHA-256 with the configured `WINDOWS_SIGNING_IDENTITY_SHA256`.
