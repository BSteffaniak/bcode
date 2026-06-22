# Release builds

Bcode release artifacts are produced by the repository `xtask` release automation and the
`release.yml` GitHub Actions workflow.

## Local commands

```sh
cargo xtask release --target aarch64-apple-darwin --version v0.1.0
cargo xtask release --target x86_64-unknown-linux-gnu --version v0.1.0
cargo xtask verify-release --target aarch64-apple-darwin --version v0.1.0
cargo xtask dev-release
```

Supported v1 targets:

* `aarch64-apple-darwin`
* `x86_64-apple-darwin`
* `aarch64-unknown-linux-gnu`
* `x86_64-unknown-linux-gnu`

Artifacts are written to `target/dist/` with adjacent `sha256` files. macOS
artifacts are `.zip` archives so Apple notarization can accept them; Linux
artifacts are `.tar.gz` archives.

## macOS release signing

macOS release builds require a stable Apple Developer ID signing identity. This
is what lets Keychain recognize updated Bcode binaries as the same trusted
program instead of repeatedly asking users to allow device-sealed credential
access.

Required local environment:

```sh
export APPLE_CODESIGN_IDENTITY="Developer ID Application: Example, Inc. (TEAMID)"
```

Optional notarization environment:

```sh
export APPLE_ID="release@example.com"
export APPLE_APP_SPECIFIC_PASSWORD="app-specific-password"
export APPLE_TEAM_ID="TEAMID"
```

Set `BCODE_SKIP_NOTARIZE=1` or pass `--skip-notarize` to skip notarization.

## macOS development signing

For local development, use a persistent local signing certificate to reduce
Keychain prompts for rebuilt binaries:

```sh
cargo xtask dev-release
```

This builds `bcode` in release mode for the host target, signs it on macOS with
the default development identity, verifies the signature, and prints the runnable
binary path.

To sign an already-built binary instead:

```sh
cargo build --release --package bcode
cargo xtask dev-sign --target aarch64-apple-darwin
```

By default, `dev-release` and `dev-sign` use a local code-signing identity
named `Bcode Dev`. If that identity does not exist yet and no override was
provided, xtask creates a dedicated Bcode development-signing keychain at:

```text
~/Library/Application Support/bcode/dev-signing/
```

The keychain password is generated locally, stored next to that keychain with
user-only file permissions, and used only to unlock the dedicated signing
keychain. The generated self-signed certificate is trusted for code signing in
that dedicated keychain. The login keychain password is not required.
Override the identity with either:

```sh
cargo xtask dev-sign --target aarch64-apple-darwin --identity "My Local Cert"
BCODE_DEV_CODESIGN_IDENTITY="My Local Cert" cargo xtask dev-sign --target aarch64-apple-darwin
```

This is not a replacement for release signing. It only helps on machines that
trust the local development certificate.

## GitHub Actions secrets

The `Release` workflow uses repository secrets to import a temporary macOS
signing keychain and sign release binaries.

Required for macOS jobs:

* `APPLE_CODESIGN_CERTIFICATE_P12_BASE64`
* `APPLE_CODESIGN_CERTIFICATE_PASSWORD`
* `APPLE_CODESIGN_IDENTITY`
* `APPLE_TEAM_ID`
* `APPLE_ID`
* `APPLE_APP_SPECIFIC_PASSWORD`

Linux jobs do not perform platform binary signing in v1. They build, package,
and checksum artifacts.

## Release workflow

Run **Release** from GitHub Actions with:

* `version`: release tag/version, such as `v0.1.0`
* `publish`: whether to create/update the GitHub release

The workflow builds macOS and Linux artifacts, uploads all artifacts, and when
`publish=true` attaches them to a GitHub release.
