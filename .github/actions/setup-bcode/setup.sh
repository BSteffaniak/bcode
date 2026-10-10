#!/usr/bin/env bash
# Install a distribution whose exact bytes are pinned by trusted workflow policy.
# This is not an arbitrary/untrusted archive installer or an OS sandbox.
set -euo pipefail
umask 077
installation=
published=false
cleanup() {
  if [[ "$published" != true && -n "$installation" ]]; then
    rm -rf -- "$installation"
  fi
}
trap cleanup EXIT
fail() { echo 'Bcode setup failed; check the trusted archive pin and runner paths' >&2; exit 1; }
[[ "${BCODE_SETUP_SHA256:-}" =~ ^[0-9a-f]{64}$ ]] || fail
[[ "${BCODE_SETUP_ARCHIVE:-}" = /* && "${RUNNER_TEMP:-}" = /* && "${GITHUB_WORKSPACE:-}" = /* && "${GITHUB_OUTPUT:-}" = /* ]] || fail
[[ -f "$BCODE_SETUP_ARCHIVE" && ! -L "$BCODE_SETUP_ARCHIVE" && -d "$RUNNER_TEMP" && -d "$GITHUB_WORKSPACE" ]] || fail
workspace=$(cd -- "$GITHUB_WORKSPACE" && pwd -P)
parent=$(cd -- "$RUNNER_TEMP" && pwd -P)
archive_parent=$(cd -- "$(dirname -- "$BCODE_SETUP_ARCHIVE")" && pwd -P)
output_parent=$(cd -- "$(dirname -- "$GITHUB_OUTPUT")" && pwd -P)
for path in "$parent" "$archive_parent" "$output_parent"; do
  [[ "$path" != "$workspace" && "$path" != "$workspace/"* && "$path" != *$'\n'* && "$path" != *$'\r'* ]] || fail
done
[[ ! -L "$GITHUB_OUTPUT" && ( ! -e "$GITHUB_OUTPUT" || -f "$GITHUB_OUTPUT" ) ]] || fail
installation=$(mktemp -d "$parent/bcode-install.XXXXXXXX")
# Hash and extract the same private snapshot. Do not execute any archive content.
cp -- "$BCODE_SETUP_ARCHIVE" "$installation/release.tar.gz"
if command -v sha256sum >/dev/null; then
  digest=$(sha256sum "$installation/release.tar.gz"); digest=${digest%% *}
else
  digest=$(shasum -a 256 "$installation/release.tar.gz"); digest=${digest%% *}
fi
[[ "$digest" = "$BCODE_SETUP_SHA256" ]] || fail
# Only reviewed, digest-pinned Bcode releases are supported. Archive structure is
# validated by release tooling; the independent pin is the bootstrap trust root.
mkdir "$installation/runtime"
tar -xzf "$installation/release.tar.gz" -C "$installation/runtime"
rm -- "$installation/release.tar.gz"
[[ -f "$installation/runtime/bcode" && ! -L "$installation/runtime/bcode" && -x "$installation/runtime/bcode" ]] || fail
printf 'installation=%s\nbinary=%s\n' "$installation/runtime" "$installation/runtime/bcode" >> "$GITHUB_OUTPUT"
published=true
