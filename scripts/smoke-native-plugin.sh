#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
server_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"

cargo build --quiet -p bcode_hello_plugin

case "$(uname -s)" in
    Darwin)
        dylib="${root}/target/debug/libbcode_hello_plugin.dylib"
        ;;
    Linux)
        dylib="${root}/target/debug/libbcode_hello_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        dylib="${root}/target/debug/bcode_hello_plugin.dll"
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

plugin_dir="${workdir}/plugins/hello"
daemon_plugin_dir="${workdir}/config/bcode/plugins/hello"
mkdir -p "${plugin_dir}" "${daemon_plugin_dir}"
cat >"${plugin_dir}/bcode-plugin.toml" <<EOF
id = "example.hello"
name = "Hello Example Plugin"
version = "0.0.1"

[[services]]
description = "Echo service used by smoke tests"
interface_id = "example-hello/v1"
name = "Hello Echo"

[runtime]
type = "native"
abi_version = 1
library = "${dylib}"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF
cp "${plugin_dir}/bcode-plugin.toml" "${daemon_plugin_dir}/bcode-plugin.toml"

export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
disabled = ["example.hello"]
EOF

if cargo run --quiet -p bcode -- plugin list --root "${workdir}/plugins" | grep -q "example.hello"; then
    echo "disabled plugin should not be listed" >&2
    exit 1
fi

cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["example.hello"]
EOF

cargo run --quiet -p bcode -- plugin list --root "${workdir}/plugins" | grep -q "example.hello"
cargo run --quiet -p bcode -- plugin services --root "${workdir}/plugins" | grep -q "example-hello/v1"
cargo run --quiet -p bcode -- plugin check --root "${workdir}/plugins" | grep -q $'example.hello\tOK'
cargo run --quiet -p bcode -- plugin invoke --root "${workdir}/plugins" example.hello example-hello/v1 echo "hello service" | grep -q "hello service"
cargo run --quiet -p bcode -- plugin call --root "${workdir}/plugins" example-hello/v1 echo "hello routed service" | grep -q "hello routed service"

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
cargo run --quiet -p bcode -- server start >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..50}; do
    if cargo run --quiet -p bcode -- server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
cargo run --quiet -p bcode -- plugin services --daemon | grep -q "example-hello/v1"
cargo run --quiet -p bcode -- plugin invoke --daemon example.hello example-hello/v1 echo "hello daemon service" | grep -q "hello daemon service"
cargo run --quiet -p bcode -- plugin call --daemon example-hello/v1 echo "hello daemon routed service" | grep -q "hello daemon routed service"
cargo run --quiet -p bcode -- server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-native-plugin: PASS"
