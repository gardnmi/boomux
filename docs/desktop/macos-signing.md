# Signed macOS distribution candidates

The preview remains experimental. Ad-hoc CI artifacts are not notarized and do
not become trusted merely because the build or checksum passes. A real clean-Mac
Gatekeeper/IME/accessibility acceptance pass is still required.

## Maintainer prerequisites

A maintainer must have accepted the relevant Apple agreements and obtained a
Developer ID Application certificate and notarization account access. This PR
neither creates those credentials nor accepts agreements. Never put credentials
in repository files, issues, logs or chat.

Before enabling signing:

1. Configure the `macos-signing` GitHub Environment with required reviewers and
   deployment branch restriction to `main`.
2. Securely configure Environment secrets `MACOS_CERTIFICATE_P12_BASE64`,
   `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGNING_IDENTITY`, `APPLE_TEAM_ID`,
   `APPLE_ID`, and `APPLE_APP_PASSWORD`. The identity must start with
   `Developer ID Application:`; Team ID is checked after signing.
3. Set repository variable `MACOS_SIGNING_ENABLED=true` only after those
   restrictions are in place. No signed job runs without this explicit opt-in.
4. Dispatch **Sign macOS distribution candidate** from `main` with a reviewed
   exact 40-character source SHA. Approve the protected signing job only after
   verifying the requested SHA and build result.

The build runs dependencies and packaged-app smoke on a separate runner without
signing secrets. The signing job checks out its scripts from the trusted default
branch. It validates bounded, symlink-free archive contents and exact source,
version, platform and bundle identity; it never executes incoming app binaries.
Only the protected job imports existing credentials, into an ephemeral keychain
removed by an EXIT trap. The small native Mach-O launcher preserves exact arguments and delegates startup to the Rust bootstrap; its signature does not depend on script extended attributes. It signs the launcher, CLI, Desktop and web gateway first, then
the whole app, with hardened runtime and secure timestamps. No `codesign --deep`
signing shortcut is used. Notarization must return Accepted, then stapling,
staple validation, strict deep signature verification and Gatekeeper assessment
must all pass before a signed ZIP/checksum can be retained.

The bundle ID remains `com.boomux.desktop.preview` across ad-hoc and signed
candidates. Changing it later is a migration decision. The app icon reuses the
existing Boomux artwork. Apple Silicon and macOS 15+ remain the only supported
preview target; this does not add Intel support.

The workflow emits a `signed-notarized-macos-candidate` artifact only. It does not
publish a GitHub release, replace existing assets, merge code, or change Linux
release jobs. Before publishing its exact bytes, verify CI and provenance for
the source SHA and complete native acceptance. Its build.json declares
`distribution=developer-id` and `notarized=true` only after all signing gates;
metadata verification alone cannot authenticate a signature. A user-initiated
Mac updater must independently check the full signature, stable bundle identity
and matching Team ID before trusting any release.

## Offline and negative acceptance

- Download with browser quarantine on a clean account, extract and open normally
- Disconnect the network and open the stapled app
- Modify each of CLI, Desktop, web gateway and a sealed resource; reject each
- Reject ad-hoc bundles and a valid signature from a different Team ID
- Test expired/revoked credentials and rejected/not-yet-finished notarization;
  neither may produce a signed-distribution artifact
- Confirm Linux archive names, contents and publication remain unchanged

Local fixtures simulate signing command order and failures. They cannot prove
Apple service acceptance, real Gatekeeper behavior, or certificate validity.
