#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

if [[ -z "${BCODE_OPENAI_API_KEY:-${OPENAI_API_KEY:-}}" ]]; then
    echo "smoke-openai-compatible-provider: SKIP (set BCODE_OPENAI_API_KEY or OPENAI_API_KEY)"
    exit 0
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-smoke.XXXXXX)"
server_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"

cargo build --quiet -p bcode --features app

cargo build --quiet -p bcode_openai_compatible_provider_plugin

case "$(uname -s)" in
    Darwin)
        dylib="${root}/target/debug/libbcode_openai_compatible_provider_plugin.dylib"
        ;;
    Linux)
        dylib="${root}/target/debug/libbcode_openai_compatible_provider_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        dylib="${root}/target/debug/bcode_openai_compatible_provider_plugin.dll"
        ;;
    *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

plugin_dir="${workdir}/config/bcode/plugins/openai-compatible"
mkdir -p "${plugin_dir}"
cat >"${plugin_dir}/bcode-plugin.toml" <<EOF
id = "bcode.openai-compatible"
name = "Bcode OpenAI-Compatible Provider"
version = "0.0.1"

[[services]]
description = "OpenAI-compatible chat completions model provider"
interface_id = "bcode.model-provider/v1"
name = "OpenAI-Compatible Model Provider"

[runtime]
type = "native"
abi_version = 2
library = "${dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
mkdir -p "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_STATE_DIR="${workdir}/state"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.openai-compatible"]

[model]
provider_plugin_id = "bcode.openai-compatible"
model_id = "${BCODE_OPENAI_MODEL:-${OPENAI_MODEL:-gpt-4.1-mini}}"
EOF

"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

"${root}/target/debug/bcode" model list | grep "${BCODE_OPENAI_MODEL:-${OPENAI_MODEL:-gpt-4.1-mini}}" >/dev/null
session_id="$("${root}/target/debug/bcode" session create openai-compatible-smoke)"
"${root}/target/debug/bcode" send "${session_id}" "Reply with exactly: bcode-openai-smoke" >/dev/null
for _ in {1..300}; do
    if "${root}/target/debug/bcode" session history "${session_id}" | grep -i "assistant:.*bcode-openai-smoke" >/dev/null >/dev/null; then
        break
    fi
    sleep 0.1
done
"${root}/target/debug/bcode" session history "${session_id}" | grep -i "assistant:.*bcode-openai-smoke" >/dev/null

"${root}/target/debug/bcode" server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-openai-compatible-provider: PASS"
