#!/usr/bin/env bash
# Run only after the coordinator releases validation. No build or paid provider.
set -euo pipefail
if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: bash $0 /absolute/identified/bcode expected-sha256 [--require-complete]" >&2
    exit 2
fi
binary="$1"
expected="$2"
mode="${3:-}"
[[ "$binary" = /* && -f "$binary" && -x "$binary" ]] || { echo 'absolute executable required' >&2; exit 2; }
[[ "$expected" =~ ^[0-9a-f]{64}$ ]] || { echo 'expected SHA-256 required' >&2; exit 2; }
[[ -z "$mode" || "$mode" == --require-complete ]] || exit 2
root="$(cd "$(dirname "$0")/.." && pwd)"
# Hash verification is before any invocation, including --version.
python3 - "$binary" "$expected" <<'PY'
import hashlib,sys
with open(sys.argv[1], 'rb') as executable:
    digest=hashlib.file_digest(executable, 'sha256').hexdigest()
if digest != sys.argv[2]:
    raise SystemExit('identified executable hash mismatch')
PY
umask 077
work="$(mktemp -d "${TMPDIR:-/tmp}/bcode-introspection.XXXXXX")"
echo "private artifacts retained: $work"
# Preserve reports on either outcome; release heavy runtime snapshots only after fencing.
cleanup_runtime() {
    local status=$?
    trap - EXIT
    python3 "$root/scripts/cleanup-eval-runtime.py" "$binary" "$work" || echo "runtime cleanup incomplete; scratch preserved" >&2
    exit "$status"
}
trap cleanup_runtime EXIT
mkdir -p "$work/home" "$work/state" "$work/xdg"
printf 'executable=%s\nsha256=%s\n' "$binary" "$expected" > "$work/artifact-identity.txt"
python3 "$root/fixtures/evals/introspection/materialize.py" "$binary" "$work/suites"
cat > "$work/config.toml" <<'EOF'
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents", "bcode.loop", "bcode.workflow", "bcode.eval"]
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-cache-explicit"
[model.prompt_cache]
mode = "off"
[observability]
persist_model_requests = false
persist_tool_io = false
EOF
run() {
    env -i PATH="$PATH" HOME="$work/home" TMPDIR="${TMPDIR:-/tmp}" \
        XDG_CONFIG_HOME="$work/xdg" BCODE_CONFIG="$work/config.toml" \
        BCODE_STATE_DIR="$work/state" "$binary" "$@"
}
# Nonzero is necessary but not sufficient: verifier checks all nine control results.
if run eval run "$work/suites/oracle-controls.toml" --output-root "$work/runs" --run-id oracle-controls --fail-under-pass-rate 1; then
    echo 'ERROR: wrong/missing independent controls accepted' >&2
    exit 1
fi
run eval run "$work/suites/offline-direct.toml" --output-root "$work/runs" --run-id offline-direct --fail-under-pass-rate 1
# Scripted structured loop provider cannot perform meaningful delegation experiments.
cat > "$work/config.toml" <<'EOF'
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents", "bcode.loop", "bcode.workflow", "bcode.eval"]
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
[model.settings]
fake_structured_output_json = "loop_until:1"
[model.prompt_cache]
mode = "off"
[observability]
persist_model_requests = false
persist_tool_io = false
[auth.profiles.judgement-fixture]
backend = "aws"
owner_plugin_id = "bcode.fake-provider"
provider_id = "fake"
scheme = "api_key"
[auth.profiles.judgement-fixture.map.api_key]
env = "FAKE_JUDGEMENT_KEY"
[auth.profiles.judgement-fixture.settings]
"env.FAKE_JUDGEMENT_KEY" = "test-only"
EOF
run eval run "$work/suites/offline-workflow.toml" --output-root "$work/runs" --run-id offline-workflow --fail-under-pass-rate 1
python3 "$root/fixtures/evals/introspection/verify_artifacts.py" "$work/runs" ${mode:+"$mode"}
