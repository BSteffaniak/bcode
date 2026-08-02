# Release builds

Bcode release artifacts are produced by the repository `xtask` release automation and the
`release.yml` GitHub Actions workflow.

## Local commands

`build` defaults to the slim `app` feature set. `release` and `dev-release` default to the complete
`distribution` feature set. Passing `--features` replaces that default composition; the required
`app` feature is always added automatically. Feature lists are comma-separated and may be repeated.

```sh
cargo xtask build
cargo xtask build --features web-renderer,static-bundled-shell-plugin
cargo xtask dev-release --features web-renderer
cargo xtask release --features static-bundled-plugins,bundled-ocr-tesseract
```

Canonical distribution commands:

```sh
cargo xtask release --target aarch64-apple-darwin --version v0.1.0
cargo xtask release --target x86_64-unknown-linux-gnu --version v0.1.0
cargo xtask release --target x86_64-pc-windows-msvc --version v0.1.0
cargo xtask verify-release --target aarch64-apple-darwin --version v0.1.0
cargo xtask dev-release
```

Supported v1 targets:

* `aarch64-apple-darwin`
* `x86_64-apple-darwin`
* `aarch64-unknown-linux-gnu`
* `x86_64-unknown-linux-gnu`
* `x86_64-pc-windows-msvc`

Artifacts are written to `target/dist/` with adjacent `sha256` files. Canonical `release` builds embed the workspace crate version as their user-visible version (`bcode v<version>`). The requested release version may have one leading `v`, but its normalized value must exactly match `[workspace.package].version`; packaging fails before building when they differ. `build` and `dev-release` remain developer builds even though they use Cargo's release profile, and report a deterministic diagnostic label containing Git state and a build digest. The display label is diagnostic only and is distinct from daemon routing identity.

Each `bcode` artifact also
contains an exact produced-artifact identity. `cargo xtask build`, `release`, and `dev-release`
generate one identity for the produced binary; signing, stripping, copying, archiving, and extraction
must preserve it. The executable's artifact identity is probed before and after host post-link
signing or stripping, and the staged copy is checked against the same value before archiving.
`verify-release` first requires exact `bcode v<workspace-version>` output from the extracted native artifact, then executes `bcode artifact-id` and fails if the identity is missing or malformed before running daemon smoke coverage. Together these checks cover
macOS signing, Linux stripping, Windows signing when configured, copy/staging, archiving, and
extraction without treating signatures or executable digests as identity. Artifact identity is
daemon routing metadata and is distinct from the archive checksum and executable SHA-256.

macOS and Windows artifacts are `.zip` archives; Linux artifacts are `.tar.gz` archives. Portable ZIP
is the complete initial Windows distribution format; an installer, Store package, package-manager
publication, and auto-updater are outside the v1 Windows milestone. Every archive contains
`bcode` and the process-isolated `bcode-mermaid-worker` (with `.exe` suffixes on Windows), plus
the bundled Tesseract runtime tree selected by the `distribution` feature. Windows source
builds use the MSVC Rust target and require Visual Studio Build Tools with C++ support and CMake.
CI verifies `cmake`, `cl.exe`, and `link.exe` explicitly and initializes the Visual Studio amd64
developer shell when those tools are not already in the runner environment.
Windows named pipes are scoped with a bounded hash of the current access-token user SID (with a
hashed normalized account/profile fallback); raw usernames are not embedded in endpoint names.
Windows x64 CI uses GitHub's `windows-latest` image, but Windows support must not be claimed until
the native workspace checks and extracted distribution smoke suite are green. The minimum supported end-user
version is Windows 10, version 1809, because Bcode's terminal shell integration depends on ConPTY.
Windows Server 2019 or newer is the corresponding server baseline.

Windows release, `dev-release`, runtime packaging, and runtime smoke commands are intentionally
native-only for the Windows target: requesting
`x86_64-pc-windows-msvc` from a non-Windows host fails before compilation with the current host in
the diagnostic. This prevents cross-compilation from creating an artifact that skipped mandatory
native execution and extracted-product validation.

### Windows source build

From a Developer PowerShell with the MSVC C++ tools available:

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --release --package bcode --bin bcode --bin bcode-mermaid-worker `
  --features distribution --target x86_64-pc-windows-msvc
cargo xtask release --target x86_64-pc-windows-msvc --version v0.1.0
cargo xtask verify-release --target x86_64-pc-windows-msvc --version v0.1.0
```

Extract the ZIP as a directory and preserve its relative layout: `bcode.exe` and
`bcode-mermaid-worker.exe` remain at the root, while the versioned OCR DLLs and language data stay
under `bcode-runtimes\\tesseract`. Run `bcode.exe --version` from the extracted directory as a basic
installation check. Release verification additionally exercises the Mermaid worker, bundled OCR,
and named-pipe daemon lifecycle. Verification is successful only when those extracted-product checks
pass on a native Windows host; creating a ZIP from another host is not evidence that the Windows
distribution is supported.

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
keychain. xtask grants `/usr/bin/codesign` access to the generated signing key
and smoke-tests that the identity can sign. It does not modify system trust
settings or require the login keychain password.
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

Linux jobs do not perform platform binary signing. Windows release jobs sign when the configured
certificate secrets are present. Public release workflow runs require Windows Authenticode signing
by default; an unsigned Windows artifact may be published only when the operator explicitly sets
`publish_windows_unsigned=true` for that workflow dispatch. An unsigned publication includes a
`WINDOWS-UNSIGNED.txt` marker beside the release assets so the exception is visible in the published
release. This override is intended for a deliberate temporary policy exception, not silent fallback.
Windows provider secrets use current-user DPAPI, and Windows shell tool commands execute with
`cmd.exe /C`; commands written for POSIX shells may need to be adapted. Build-only or unpublished
Windows artifacts are unsigned when no signing certificate is configured. For signed public
releases, configure `WINDOWS_CODESIGN_CERTIFICATE_PFX_BASE64` and
`WINDOWS_CODESIGN_CERTIFICATE_PASSWORD` as GitHub Actions secrets. The workflow decodes the PFX to
the runner's temporary directory and passes only its path to xtask through
`WINDOWS_CODESIGN_CERTIFICATE_PFX_PATH`; release automation signs and RFC 3161 timestamps both
executables with SHA-256 before packaging, verifies both signatures and their RFC 3161 timestamps
before packaging, and verifies them again after extraction (`signtool verify /pa /all /tw /v`). The
workflow writes a versioned `windows-signing-x86_64-pc-windows-msvc.json` provenance record for
public Windows assets. The publish job requires that record to report either signed, timestamped,
pre/post-package verification or the explicit unsigned exception before attaching any files.
Public signing requires an explicit operator decision before credentials are configured: select an
Authenticode code-signing certificate or managed signing service whose CI interface can preserve
this workflow's pre-package timestamp and post-extraction verification contract. Record the chosen
provider, certificate subject/thumbprint ownership, repository or environment secret administrators,
renewal owner/date, and emergency revocation contact in the release runbook. Do not place PFX bytes,
passwords, provider tokens, or private-key material in the repository, workflow inputs, artifacts, or
logs. Until that decision and a successful signed publication exist, use unsigned artifacts only via
the explicit exception and do not claim production Authenticode coverage.

Store PFX as an Actions secret encoded or supplied in the format
expected by the runner secret policy; the workflow rejects incomplete certificate/password secret
pairs and removes the temporary PFX in an `always()` cleanup step. Rotate or revoke it through the
certificate authority and replace the repository secrets. Local development remains unsigned unless these variables are set.
A valid Authenticode signature does not guarantee Microsoft Defender SmartScreen reputation.

Canonical distribution builds also provide diagnostic release metadata to `bcode /version`:

* `BCODE_RELEASE_CHANNEL` defaults to `stable` in `cargo xtask release` and may be explicitly overridden.
* `SOURCE_DATE_EPOCH` is the reproducible build/release timestamp. When absent, release automation uses the source commit timestamp when Git metadata is available.
* Local builds do not use the wall clock and report the build date as unavailable unless `SOURCE_DATE_EPOCH` was explicitly supplied.

The timestamp and channel are diagnostic only. They do not enter daemon routing, compatibility, or the deterministic developer build digest.

## Release workflow

Run **Release** from GitHub Actions with:

* `version`: release tag/version, such as `v0.1.0`
* `publish`: whether to create/update the GitHub release
* `publish_windows_unsigned`: explicit exception allowing an unsigned Windows artifact when
  `publish=true`; leave false to require configured Authenticode credentials

The build matrix has read-only repository permissions; only the gated publish job receives
`contents: write`. The workflow builds macOS, Linux, and Windows x64 artifacts, uploads all artifacts with a 14-day
retention period, and when `publish=true` attaches them to a GitHub release. Release runs for the
same version are serialized and are not automatically cancelled, avoiding two publishers racing to
replace the same assets. Allowing unsigned publication is a fallback: if valid signed provenance is
present, the signed artifact is published and no unsigned marker is emitted.
