#!/usr/bin/env bash
set -euo pipefail

# Offline image artifact/session probe. No user config, credentials, or state are inherited.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_root="${TMPDIR:-/tmp}"
workdir="$(mktemp -d "${tmp_root%/}/bcode-image-eval.XXXXXX")"
echo "image eval artifacts: ${workdir}"
cd "$root"
cargo build -p bcode --bin bcode --features app,static-bundled-plugins,static-bundled-fake-provider-plugin
mkdir -p "${workdir}/home" "${workdir}/xdg" "${workdir}/state" "${workdir}/workspace"
# A real PNG, no Pillow or downloaded fixture required. Red left panel, blue right panel.
python3 - "${workdir}/workspace/panels.png" <<'PY'
import pathlib, struct, sys, zlib

def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
row = b'\x00' + bytes([255, 0, 0]) * 128 + bytes([0, 0, 255]) * 128
png = b'\x89PNG\r\n\x1a\n'
png += chunk(b'IHDR', struct.pack('>IIBBBBB', 256, 128, 8, 2, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress(row * 128)) + chunk(b'IEND', b'')
with open(sys.argv[1], 'wb') as output:
    output.write(png)
with pathlib.Path(sys.argv[1]).with_name('corrupt.png').open('wb') as output:
    output.write(png[:33])  # Valid signature/dimensions, missing pixel stream.
PY
cat >"${workdir}/bcode.toml" <<'EOF'
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents"]
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-vision-panels"
[model.prompt_cache]
mode = "off"
EOF
cat >"${workdir}/suite.toml" <<'EOF'
schema_version = 1
id = "image-artifact-replay"
name = "Image artifact replay"
[run]
repetitions = 1
timeout_ms = 120000
isolation = "temp_copy"
randomize_case_order = false
fail_fast = false
[[variants]]
id = "inline"
name = "Tool image with restart"
executor = "agent"
allowed_tools = ["filesystem.read"]
metadata = { agent_id = "build", permission_mode = "approve" }
[variants.follow_up]
prompt = "Name the panel colors again, left to right."
restart_daemon = true
[[cases]]
id = "panels"
name = "Read PNG panels"
fixture = "workspace"
prompt = "tool-read panels.png"
timeout_ms = 120000
[[cases.judges]]
type = "metric_threshold"
metric = "tool_call_count"
min = 1
max = 1
required = true
[[cases.judges]]
type = "metric_threshold"
metric = "tool_error_count"
max = 0
required = true

[[cases]]
id = "missing-image"
name = "Missing image does not become visual context"
fixture = "workspace"
prompt = "tool-read absent.png"
timeout_ms = 120000
[[cases.judges]]
type = "metric_threshold"
metric = "tool_call_count"
min = 1
max = 1
required = true
[[cases.judges]]
type = "metric_threshold"
metric = "tool_error_count"
min = 1
required = true

[[cases]]
id = "corrupt-image"
name = "Corrupt image is rejected"
fixture = "workspace"
prompt = "tool-read corrupt.png"
timeout_ms = 120000
[[cases.judges]]
type = "metric_threshold"
metric = "tool_call_count"
min = 1
max = 1
required = true
[[cases.judges]]
type = "metric_threshold"
metric = "tool_error_count"
min = 1
required = true
EOF
run() {
    env -i PATH="${PATH}" HOME="${workdir}/home" TMPDIR="${tmp_root}" \
        XDG_CONFIG_HOME="${workdir}/xdg" BCODE_CONFIG="${workdir}/bcode.toml" \
        BCODE_STATE_DIR="${workdir}/state" "${root}/target/debug/bcode" "$@"
}
run eval validate "${workdir}/suite.toml"
run eval run "${workdir}/suite.toml" --output-root "${workdir}/runs" --run-id offline-images --fail-under-pass-rate 1.0
# Judge normalized exported session events, not arbitrary strings in tool output or traces.
python3 - "${workdir}/runs/offline-images/cases" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
for case, expected, outcome in [('panels', 'red blue', 'completed'),
                                ('missing-image', 'UNKNOWN', 'completed'),
                                ('corrupt-image', 'UNKNOWN', 'completed')]:
    for name in ('transcript.jsonl', 'follow-up-transcript.jsonl'):
        text, outcomes, failed_invocations = [], [], set()
        path = root / case / 'variants/inline/repetitions/0001' / name
        with path.open() as stream:
            for line in stream:
                event = json.loads(line)
                kind = event['kind']
                if 'positioned_assistant_response_segment' in kind:
                    text.append(kind['positioned_assistant_response_segment']['text'])
                if 'tool_invocation_result_recorded' in kind:
                    record = kind['tool_invocation_result_recorded']['record']
                    if record['is_error']:
                        failed_invocations.add(record['invocation_id'])
                if 'model_turn_finished' in kind:
                    outcomes.append(kind['model_turn_finished']['outcome'])
        if name == 'transcript.jsonl':
            assert len(failed_invocations) == (0 if case == 'panels' else 1), (case, failed_invocations)
        assert outcomes == [outcome], (case, name, outcomes)
        assert ''.join(text).strip() == expected, (case, name, text)
print('pixel answers and missing/corrupt-image behavior verified before and after daemon restart')
PY
# Exercise the matrix orchestration against the explicitly isolated fake provider, not a
# credentialed provider. --live means execute rather than dry-run; this config is offline.
env -i PATH="${PATH}" HOME="${workdir}/home" TMPDIR="${tmp_root}" \
    XDG_CONFIG_HOME="${workdir}/xdg" BCODE_STATE_DIR="${workdir}/state" \
    PYTHONDONTWRITEBYTECODE=1 python3 "${root}/scripts/verify-image-matrix.py" \
    --bcode "${root}/target/debug/bcode" --config "${workdir}/bcode.toml" \
    --model fake-vision-panels --seed 726 --seed 451 --live
echo "image eval passed; retained artifacts: ${workdir}/runs/offline-images"
