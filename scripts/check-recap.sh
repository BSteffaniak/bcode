#!/usr/bin/env bash
# Offline, real-media acceptance; no provider credentials, installs, or downloads.
# Invoke only when this checkout's build coordinator has granted the Cargo slot.
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in ffmpeg ffprobe; do
    command -v "$tool" >/dev/null || { echo "Required local tool unavailable: $tool" >&2; exit 2; }
done
if [[ -z "${RECAP_ACCEPTANCE_OUTPUT:-}" ]]; then
    RECAP_ACCEPTANCE_OUTPUT="$(mktemp -d "${TMPDIR:-/tmp}/recap-acceptance.XXXXXX")"
fi
export RECAP_ACCEPTANCE_OUTPUT
mkdir -p "$RECAP_ACCEPTANCE_OUTPUT"
RECAP_ACCEPTANCE_OUTPUT="$(cd "$RECAP_ACCEPTANCE_OUTPUT" && pwd)"
export RECAP_ACCEPTANCE_OUTPUT
cargo test -p bcode_recap --test acceptance -- --nocapture
if [[ "${RECAP_SKIP_BROWSER:-0}" == 1 ]]; then
    echo "Browser acceptance explicitly skipped; not browser-validated. HTML: $RECAP_ACCEPTANCE_OUTPUT/recap.html"
else
    command -v node >/dev/null || { echo 'Node unavailable; cannot run browser acceptance.' >&2; exit 2; }
    node packages/recap/tests/browser.cjs "$RECAP_ACCEPTANCE_OUTPUT"
fi
printf 'Recap acceptance artifacts: %s\n' "$RECAP_ACCEPTANCE_OUTPUT"
