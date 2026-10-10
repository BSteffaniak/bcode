#!/usr/bin/env bash
# Behavioral tests for the trusted-release bootstrap; no provider or network use.
set -euo pipefail
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
mkdir "$root/workspace" "$root/source" "$root/runner"
printf '#!/bin/sh\nexit 0\n' > "$root/source/bcode"
chmod 700 "$root/source/bcode"
tar -czf "$root/release.tar.gz" -C "$root/source" .
if command -v sha256sum >/dev/null; then
  pin=$(sha256sum "$root/release.tar.gz"); pin=${pin%% *}
else
  pin=$(shasum -a 256 "$root/release.tar.gz"); pin=${pin%% *}
fi
adapter=$(cd -- "$(dirname -- "$0")/.." && pwd)/.github/actions/setup-bcode/setup.sh
invoke() {
  BCODE_SETUP_ARCHIVE="$root/release.tar.gz" BCODE_SETUP_SHA256="$1" \
    GITHUB_WORKSPACE="$root/workspace" RUNNER_TEMP="$root/runner" GITHUB_OUTPUT="$2" \
    bash "$adapter"
}
invoke "$pin" "$root/output"
binary=$(grep '^binary=' "$root/output"); binary=${binary#binary=}
[[ -x "$binary" ]] && cmp "$binary" "$root/source/bcode"
[[ $(find "$root/runner" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ') = 1 ]]
if invoke "$(printf '%064d' 0)" "$root/rejected"; then exit 1; fi
[[ ! -e "$root/rejected" ]]
[[ $(find "$root/runner" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ') = 1 ]]
printf 'preserve\n' > "$root/workspace/output"
if invoke "$pin" "$root/workspace/output"; then exit 1; fi
[[ $(cat "$root/workspace/output") = preserve ]]
ln -s "$root/output" "$root/output-link"
if invoke "$pin" "$root/output-link"; then exit 1; fi
if invoke "$pin" relative-output; then exit 1; fi
printf 'Bcode setup behavior tests passed\n'
