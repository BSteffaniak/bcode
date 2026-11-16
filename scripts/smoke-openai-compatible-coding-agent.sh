#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-smoke.XXXXXX)"
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

cargo build --quiet -p bcode --features distribution

cat >"${workdir}/mock_openai_coding.js" <<'JS'
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

function hasTool(payload, name) {
  return (payload.tools || []).some((tool) => tool.function && tool.function.name === name);
}

function latestToolContent(payload) {
  const toolMessages = (payload.messages || []).filter((message) => message.role === 'tool');
  if (!toolMessages.length) {
    return '';
  }
  return toolMessages[toolMessages.length - 1].content || '';
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
      if (!hasTool(payload, 'filesystem_write') || !hasTool(payload, 'shell_run')) {
        response.writeHead(400);
        response.end(`missing coding tools: ${body}`);
        return;
      }
      sendSse(response, [
        { choices: [{ delta: { content: 'I will write the requested file, then validate it.\n' }, finish_reason: null }] },
        { choices: [{ delta: { tool_calls: [{ index: 0, id: 'call_write', function: { name: 'filesystem_write', arguments: '' } }] }, finish_reason: null }] },
        { choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: JSON.stringify({ path: targetFile, contents: 'dogfood smoke ok\n' }) } }] }, finish_reason: null }] },
        { choices: [{ delta: {}, finish_reason: 'tool_calls' }] },
      ]);
      return;
    }

    if (requests === 2) {
      if (!latestToolContent(payload).includes('wrote')) {
        response.writeHead(400);
        response.end(`missing write tool result: ${body}`);
        return;
      }
      sendSse(response, [
        { choices: [{ delta: { tool_calls: [{ index: 0, id: 'call_validate', function: { name: 'shell_run', arguments: '' } }] }, finish_reason: null }] },
        { choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: JSON.stringify({ command: `grep -q 'dogfood smoke ok' '${targetFile}' && printf validation-ok`, timeout_ms: 5000 }) } }] }, finish_reason: null }] },
        { choices: [{ delta: {}, finish_reason: 'tool_calls' }] },
      ]);
      return;
    }

    if (!latestToolContent(payload).includes('validation-ok')) {
      response.writeHead(400);
      response.end(`missing validation result: ${body}`);
      return;
    }
    sendSse(response, [
      { choices: [{ delta: { content: 'bcode-coding-smoke-ok' }, finish_reason: null }] },
      { choices: [{ delta: {}, finish_reason: 'stop' }] },
    ]);
  });
});

server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync(portFile, String(server.address().port));
});
JS

target_file="${workdir}/repo/generated.txt"
node "${workdir}/mock_openai_coding.js" "${workdir}/mock-port" "${target_file}" >"${workdir}/mock.log" 2>&1 &
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

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
mkdir -p "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_STATE_DIR="${workdir}/state"
export BCODE_OPENAI_API_KEY="mock-key"
export BCODE_OPENAI_BASE_URL="http://127.0.0.1:${mock_port}"
export BCODE_OPENAI_MODEL="mock-coding-model"
export BCODE_OPENAI_MODELS="mock-coding-model"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.openai-compatible", "bcode.filesystem", "bcode.shell"]

[model]
provider_plugin_id = "bcode.openai-compatible"
model_id = "mock-coding-model"

[agent.build.permission]
external_directory = "allow"
command = { "*" = "ask", "grep -q *" = "allow", "printf validation-ok" = "allow" }
write = { "**" = "allow" }
EOF

"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

session_id="$("${root}/target/debug/bcode" session create openai-coding-smoke)"
"${root}/target/debug/bcode" send "${session_id}" "create the dogfood smoke file and validate it" >/dev/null
for _ in {1..150}; do
    if "${root}/target/debug/bcode" session history "${session_id}" | grep "bcode-coding-smoke-ok" >/dev/null; then
        break
    fi
    sleep 0.1
done

[[ -f "${target_file}" ]]
grep -q "dogfood smoke ok" "${target_file}"
"${root}/target/debug/bcode" session history "${session_id}" | grep "filesystem.write" >/dev/null
"${root}/target/debug/bcode" session history "${session_id}" | grep "shell.run" >/dev/null
"${root}/target/debug/bcode" session history "${session_id}" | grep "validation-ok" >/dev/null
"${root}/target/debug/bcode" session history "${session_id}" | grep "bcode-coding-smoke-ok" >/dev/null

"${root}/target/debug/bcode" server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-openai-compatible-coding-agent: PASS"
