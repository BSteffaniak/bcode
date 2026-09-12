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
import struct, sys, zlib

def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
row = b'\x00' + bytes([255, 0, 0]) * 128 + bytes([0, 0, 255]) * 128
png = b'\x89PNG\r\n\x1a\n'
png += chunk(b'IHDR', struct.pack('>IIBBBBB', 256, 128, 8, 2, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress(row * 128)) + chunk(b'IEND', b'')
with open(sys.argv[1], 'wb') as output:
    output.write(png)
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
EOF
run() {
    env -i PATH="${PATH}" HOME="${workdir}/home" TMPDIR="${tmp_root}" \
        XDG_CONFIG_HOME="${workdir}/xdg" BCODE_CONFIG="${workdir}/bcode.toml" \
        BCODE_STATE_DIR="${workdir}/state" "${root}/target/debug/bcode" "$@"
}
run eval validate "${workdir}/suite.toml"
run eval run "${workdir}/suite.toml" --output-root "${workdir}/runs" --run-id offline-images --fail-under-pass-rate 1.0
# Judge normalized exported session events, not arbitrary strings in tool output or traces.
python3 - "${workdir}/runs/offline-images/cases/panels/variants/inline/repetitions/0001" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
for name in ('transcript.jsonl', 'follow-up-transcript.jsonl'):
    text, outcomes = [], []
    with (root / name).open() as stream:
        for line in stream:
            event = json.loads(line)
            kind = event['kind']
            if 'positioned_assistant_response_segment' in kind:
                text.append(kind['positioned_assistant_response_segment']['text'])
            if 'model_turn_finished' in kind:
                outcomes.append(kind['model_turn_finished']['outcome'])
    assert outcomes == ['completed'], (name, outcomes)
    assert ''.join(text).strip() == 'red blue', (name, text)
print('pixel answers verified before and after daemon restart')
PY
echo "image eval passed; retained artifacts: ${workdir}/runs/offline-images"
