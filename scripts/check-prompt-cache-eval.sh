#!/usr/bin/env bash
set -euo pipefail

# Run the prompt-cache eval suite offline against the fake provider's simulated cache models.
# Exercises host cache planning, per-round cache telemetry, cross-variant comparisons, and
# session resume after daemon restart without credentials.

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_root="${TMPDIR:-/tmp}"
workdir="$(mktemp -d "${tmp_root%/}/bcode-prompt-cache-eval.XXXXXX")"
model="${BCODE_PROMPT_CACHE_EVAL_MODEL:-fake-cache-explicit}"

cargo build -p bcode --bin bcode --features app,static-bundled-plugins,static-bundled-fake-provider-plugin
bcode="${root}/target/debug/bcode"

cat >"${workdir}/bcode.toml" <<EOF
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "${model}"
EOF
mkdir -p "${workdir}/home" "${workdir}/xdg" "${workdir}/state"

# Evals own isolated process state: run with a scrubbed environment so no BCODE_* setting,
# provider credential, or session store from the invoking shell leaks into the daemon.
run() {
    env -i \
        PATH="${PATH}" \
        HOME="${workdir}/home" \
        TMPDIR="${tmp_root}" \
        XDG_CONFIG_HOME="${workdir}/xdg" \
        BCODE_CONFIG="${workdir}/bcode.toml" \
        BCODE_STATE_DIR="${workdir}/state" \
        "${bcode}" "$@"
}

run eval validate "${root}/fixtures/evals/prompt-cache/suite.toml"
if run eval run "${root}/fixtures/evals/prompt-cache/suite.toml" \
    --output-root "${workdir}/runs" \
    --run-id ci-prompt-cache \
    --fail-under-pass-rate 1.0; then
    rm -rf "${workdir}"
    echo "prompt cache eval passed (${model})"
else
    echo "prompt cache eval failed (${model}); artifacts kept at ${workdir}/runs/ci-prompt-cache" >&2
    exit 1
fi
