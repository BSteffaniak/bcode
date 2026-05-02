#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${BCODE_OPENAI_API_KEY:-${OPENAI_API_KEY:-}}" ]]; then
    echo "smoke-openai-compatible-provider: SKIP (set BCODE_OPENAI_API_KEY or OPENAI_API_KEY)"
    exit 0
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
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
abi_version = 1
library = "${dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.openai-compatible"]

[model]
model_id = "${BCODE_OPENAI_MODEL:-${OPENAI_MODEL:-gpt-4.1-mini}}"
EOF

cargo run --quiet -p bcode -- server start >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if cargo run --quiet -p bcode -- server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

cargo run --quiet -p bcode -- model list | grep -q "${BCODE_OPENAI_MODEL:-${OPENAI_MODEL:-gpt-4.1-mini}}"
session_id="$(cargo run --quiet -p bcode -- session create openai-compatible-smoke)"
cargo run --quiet -p bcode -- send "${session_id}" "Reply with exactly: bcode-openai-smoke" >/dev/null
cargo run --quiet -p bcode -- session history "${session_id}" | grep -qi "bcode-openai-smoke"

cargo run --quiet -p bcode -- server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-openai-compatible-provider: PASS"
