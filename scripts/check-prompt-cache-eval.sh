#!/usr/bin/env bash
set -euo pipefail

# Run the prompt-cache eval suite offline against the fake provider's simulated cache models.
# Exercises host cache planning, per-round cache telemetry, cross-variant comparisons, and
# session resume after daemon restart without credentials.

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_root="${TMPDIR:-/tmp}"
workdir="$(mktemp -d "${tmp_root%/}/bcode-prompt-cache-eval.XXXXXX")"
model="${BCODE_PROMPT_CACHE_EVAL_MODEL:-fake-cache-explicit}"

# An explicit binary lets callers validate an identified release artifact. Do not
# rebuild a different binary and then accidentally report that build as tested.
if [[ -n "${BCODE_PROMPT_CACHE_EVAL_BINARY:-}" ]]; then
    bcode="${BCODE_PROMPT_CACHE_EVAL_BINARY}"
    if [[ "${bcode}" != /* || ! -x "${bcode}" ]]; then
        echo "BCODE_PROMPT_CACHE_EVAL_BINARY must name an executable absolute path" >&2
        exit 1
    fi
else
    cargo build -p bcode --bin bcode --features app,static-bundled-plugins,static-bundled-fake-provider-plugin
    bcode="${root}/target/debug/bcode"
fi
printf 'prompt cache eval binary: %s\n' "${bcode}"
# Preserve the tested executable's identity beside the reports, including when
# an externally supplied release is older than the checkout driving the suite.
{
    printf 'binary: %s\n' "${bcode}"
    "${bcode}" --version
    printf 'fixture revision: '
    git -C "${root}" rev-parse HEAD
    printf 'fixture checkout status:\n'
    git -C "${root}" status --short
    printf 'model: %s\n' "${model}"
} >"${workdir}/artifact-identity.txt"
cat "${workdir}/artifact-identity.txt"

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

suite="${root}/fixtures/evals/prompt-cache/suite.toml"
if [[ "${model}" == "fake-cache-prefix" ]]; then
    suite="${root}/fixtures/evals/prompt-cache/prefix.toml"
fi
run eval validate "${suite}"
if run eval run "${suite}" \
    --output-root "${workdir}/runs" \
    --run-id ci-prompt-cache \
    --fail-under-pass-rate 1.0; then
    compaction_suite="${root}/fixtures/evals/prompt-cache/compaction.toml"
    run eval validate "${compaction_suite}"
    if ! run eval run "${compaction_suite}" \
        --output-root "${workdir}/runs" \
        --run-id ci-prompt-cache-compaction \
        --fail-under-pass-rate 1.0; then
        echo "compaction cache eval failed (${model}); artifacts kept at ${workdir}/runs/ci-prompt-cache-compaction" >&2
        exit 1
    fi
    if [[ "${BCODE_PROMPT_CACHE_EVAL_KEEP_ARTIFACTS:-0}" == "1" ]]; then
        echo "prompt cache eval artifacts: ${workdir}/runs/ci-prompt-cache"
    else
        rm -rf "${workdir}"
    fi
    echo "prompt cache eval passed (${model})"
else
    echo "prompt cache eval failed (${model}); artifacts kept at ${workdir}/runs/ci-prompt-cache" >&2
    exit 1
fi
