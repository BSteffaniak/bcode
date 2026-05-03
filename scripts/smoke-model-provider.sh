#!/usr/bin/env bash
set -euo pipefail

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

cargo build --quiet -p bcode_fake_provider_plugin

case "$(uname -s)" in
    Darwin)
        dylib="${root}/target/debug/libbcode_fake_provider_plugin.dylib"
        ;;
    Linux)
        dylib="${root}/target/debug/libbcode_fake_provider_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        dylib="${root}/target/debug/bcode_fake_provider_plugin.dll"
        ;;
    *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

if [[ ! -f "${dylib}" ]]; then
    echo "plugin library was not built: ${dylib}" >&2
    exit 1
fi

plugin_dir="${workdir}/config/bcode/plugins/fake-provider"
mkdir -p "${plugin_dir}"
cat >"${plugin_dir}/bcode-plugin.toml" <<EOF
id = "bcode.fake-provider"
name = "Bcode Fake Model Provider"
version = "0.0.1"

[[services]]
description = "Deterministic model provider used by tests and smoke flows"
interface_id = "bcode.model-provider/v1"
name = "Fake Model Provider"

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
enabled = ["bcode.fake-provider"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
EOF

cargo run --quiet -p bcode -- server start >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if cargo run --quiet -p bcode -- server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

cargo run --quiet -p bcode -- plugin services --daemon | grep -q "bcode.model-provider/v1"
cargo run --quiet -p bcode -- server status | grep -q "model provider: bcode.fake-provider"
cargo run --quiet -p bcode -- server status | grep -q "model: fake-echo"
cargo run --quiet -p bcode -- model list | grep -q "fake-echo"
cargo run --quiet -p bcode -- model capabilities | grep -q "bcode.fake-provider"
session_id="$(cargo run --quiet -p bcode -- session create model-provider-smoke)"
cargo run --quiet -p bcode -- send "${session_id}" "hello model" >/dev/null
if ! cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "fake: hello model"; then
    echo "model provider response was not recorded" >&2
    echo "--- history ---" >&2
    cargo run --quiet -p bcode -- session history "${session_id}" >&2 || true
    echo "--- server log ---" >&2
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi

cargo run --quiet -p bcode -- server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-model-provider: PASS"
