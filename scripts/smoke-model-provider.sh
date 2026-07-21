#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

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
enabled = ["bcode.fake-provider"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
EOF

"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

"${root}/target/debug/bcode" plugin services --daemon | grep "bcode.model-provider/v1" >/dev/null
"${root}/target/debug/bcode" server status | grep "model provider: bcode.fake-provider" >/dev/null
"${root}/target/debug/bcode" server status | grep "model: fake-echo" >/dev/null
"${root}/target/debug/bcode" model list | grep "fake-echo" >/dev/null
"${root}/target/debug/bcode" model capabilities | grep "bcode.fake-provider" >/dev/null
session_id="$("${root}/target/debug/bcode" session create model-provider-smoke)"
"${root}/target/debug/bcode" model set "${session_id}" --provider bcode.fake-provider fake-echo | grep "session model set" >/dev/null
"${root}/target/debug/bcode" session history "${session_id}" | grep "model changed: bcode.fake-provider/fake-echo" >/dev/null
"${root}/target/debug/bcode" send "${session_id}" "hello model" >/dev/null
for _ in {1..100}; do
    if "${root}/target/debug/bcode" session history "${session_id}" | grep "fake: hello model" >/dev/null; then
        break
    fi
    sleep 0.1
done
if ! "${root}/target/debug/bcode" session history "${session_id}" | grep "fake: hello model" >/dev/null; then
    echo "model provider response was not recorded" >&2
    echo "--- history ---" >&2
    "${root}/target/debug/bcode" session history "${session_id}" >&2 || true
    echo "--- server log ---" >&2
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi

"${root}/target/debug/bcode" server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-model-provider: PASS"
