#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
server_pid=""
mock_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    if [[ -n "${mock_pid}" ]] && kill -0 "${mock_pid}" 2>/dev/null; then
        kill "${mock_pid}" 2>/dev/null || true
        wait "${mock_pid}" 2>/dev/null || true
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"

cargo build --quiet -p bcode_openai_compatible_provider_plugin -p bcode_filesystem_plugin

case "$(uname -s)" in
    Darwin)
        openai_dylib="${root}/target/debug/libbcode_openai_compatible_provider_plugin.dylib"
        fs_dylib="${root}/target/debug/libbcode_filesystem_plugin.dylib"
        ;;
    Linux)
        openai_dylib="${root}/target/debug/libbcode_openai_compatible_provider_plugin.so"
        fs_dylib="${root}/target/debug/libbcode_filesystem_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        openai_dylib="${root}/target/debug/bcode_openai_compatible_provider_plugin.dll"
        fs_dylib="${root}/target/debug/bcode_filesystem_plugin.dll"
        ;;
    *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

cat >"${workdir}/mock_openai.js" <<'JS'
const fs = require('fs');
const http = require('http');

const portFile = process.argv[2];
const targetFile = process.argv[3];
let requests = 0;

function sendSse(response, chunks) {
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  for (const chunk of chunks) {
    response.write(`data: ${JSON.stringify(chunk)}\n\n`);
  }
  response.write('data: [DONE]\n\n');
  response.end();
}

const server = http.createServer((request, response) => {
  if (request.method !== 'POST' || request.url !== '/chat/completions') {
    response.writeHead(404);
    response.end();
    return;
  }
  let body = '';
  request.on('data', (chunk) => { body += chunk; });
  request.on('end', () => {
    const payload = JSON.parse(body || '{}');
    requests += 1;
    if (requests === 1) {
      if (!(payload.tools || []).some((tool) => tool.function && tool.function.name === 'filesystem_read')) {
        response.writeHead(400);
        response.end(`missing filesystem_read tool: ${body}`);
        return;
      }
      sendSse(response, [
        { choices: [{ delta: { tool_calls: [{ index: 0, id: 'call_1', function: { name: 'filesystem_read', arguments: '' } }] }, finish_reason: null }] },
        { choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: JSON.stringify({ path: targetFile }) } }] }, finish_reason: null }] },
        { choices: [{ delta: {}, finish_reason: 'tool_calls' }] },
      ]);
      return;
    }
    const toolMessages = (payload.messages || []).filter((message) => message.role === 'tool');
    if (!toolMessages.length || !(toolMessages[toolMessages.length - 1].content || '').includes('mock-openai-tool-content')) {
      response.writeHead(400);
      response.end(`missing tool result: ${body}`);
      return;
    }
    sendSse(response, [
      { choices: [{ delta: { content: 'mock-openai-tool-loop-ok' }, finish_reason: null }] },
      { choices: [{ delta: {}, finish_reason: 'stop' }] },
    ]);
  });
});

server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync(portFile, String(server.address().port));
});
JS

target_file="${workdir}/tool-input.txt"
printf 'mock-openai-tool-content' >"${target_file}"
node "${workdir}/mock_openai.js" "${workdir}/mock-port" "${target_file}" >"${workdir}/mock.log" 2>&1 &
mock_pid="$!"
for _ in {1..100}; do
    [[ -s "${workdir}/mock-port" ]] && break
    sleep 0.05
done
if [[ ! -s "${workdir}/mock-port" ]]; then
    echo "mock OpenAI server did not start" >&2
    cat "${workdir}/mock.log" >&2 || true
    exit 1
fi
mock_port="$(cat "${workdir}/mock-port")"

openai_dir="${workdir}/config/bcode/plugins/openai-compatible"
fs_dir="${workdir}/config/bcode/plugins/filesystem"
mkdir -p "${openai_dir}" "${fs_dir}"
cat >"${openai_dir}/bcode-plugin.toml" <<EOF
id = "bcode.openai-compatible"
name = "Bcode OpenAI-Compatible Provider"
version = "0.0.1"

[[services]]
interface_id = "bcode.model-provider/v1"
name = "OpenAI-Compatible Model Provider"

[runtime]
type = "native"
abi_version = 1
library = "${openai_dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF
cat >"${fs_dir}/bcode-plugin.toml" <<EOF
id = "bcode.filesystem"
name = "Bcode Filesystem Plugin"
version = "0.0.1"

[[services]]
interface_id = "bcode.filesystem/v1"
name = "Filesystem"

[[services]]
interface_id = "bcode.tool/v1"
name = "Filesystem Tools"

[runtime]
type = "native"
abi_version = 1
library = "${fs_dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
export BCODE_OPENAI_API_KEY="mock-key"
export BCODE_OPENAI_BASE_URL="http://127.0.0.1:${mock_port}"
export BCODE_OPENAI_MODEL="mock-model"
export BCODE_OPENAI_MODELS="mock-model,mock-tool-model"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.openai-compatible", "bcode.filesystem"]

[model]
provider_plugin_id = "bcode.openai-compatible"
model_id = "mock-model"
EOF

cargo run --quiet -p bcode -- server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if cargo run --quiet -p bcode -- server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

cargo run --quiet -p bcode -- model list | grep -q "mock-tool-model"
session_id="$(cargo run --quiet -p bcode -- session create openai-tool-smoke)"
cargo run --quiet -p bcode -- send "${session_id}" "read the provided file using the filesystem tool" >/dev/null
for _ in {1..100}; do
    if cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "mock-openai-tool-loop-ok"; then
        break
    fi
    sleep 0.1
done
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "filesystem.read"
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "mock-openai-tool-content"
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "mock-openai-tool-loop-ok"

cargo run --quiet -p bcode -- server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-openai-compatible-tool-call: PASS"
