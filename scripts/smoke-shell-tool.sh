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

cargo build --quiet -p bcode_fake_provider_plugin -p bcode_shell_plugin

case "$(uname -s)" in
    Darwin)
        fake_dylib="${root}/target/debug/libbcode_fake_provider_plugin.dylib"
        shell_dylib="${root}/target/debug/libbcode_shell_plugin.dylib"
        ;;
    Linux)
        fake_dylib="${root}/target/debug/libbcode_fake_provider_plugin.so"
        shell_dylib="${root}/target/debug/libbcode_shell_plugin.so"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        fake_dylib="${root}/target/debug/bcode_fake_provider_plugin.dll"
        shell_dylib="${root}/target/debug/bcode_shell_plugin.dll"
        ;;
    *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

fake_dir="${workdir}/config/bcode/plugins/fake-provider"
shell_dir="${workdir}/config/bcode/plugins/shell"
mkdir -p "${fake_dir}" "${shell_dir}"
cat >"${fake_dir}/bcode-plugin.toml" <<EOF
id = "bcode.fake-provider"
name = "Bcode Fake Model Provider"
version = "0.0.1"

[[services]]
interface_id = "bcode.model-provider/v1"
name = "Fake Model Provider"

[runtime]
type = "native"
abi_version = 1
library = "${fake_dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF
cat >"${shell_dir}/bcode-plugin.toml" <<EOF
id = "bcode.shell"
name = "Bcode Shell Plugin"
version = "0.0.1"

[[services]]
interface_id = "bcode.tool/v1"
name = "Shell Tools"

[runtime]
type = "native"
abi_version = 1
library = "${shell_dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.fake-provider", "bcode.shell"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
EOF

cargo run --quiet -p bcode -- server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if cargo run --quiet -p bcode -- server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

session_id="$(cargo run --quiet -p bcode -- session create shell-smoke)"
cargo run --quiet -p bcode -- send "${session_id}" "tool-shell printf shell-smoke-output" >/dev/null
permission_id=""
for _ in {1..50}; do
    permission_id="$(cargo run --quiet -p bcode -- permission list | awk 'NR == 1 { print $1 }')"
    if [[ -n "${permission_id}" ]]; then
        break
    fi
    sleep 0.1
done
if [[ -z "${permission_id}" ]]; then
    echo "shell permission request was not recorded" >&2
    cargo run --quiet -p bcode -- session history "${session_id}" >&2 || true
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi
cargo run --quiet -p bcode -- permission approve "${permission_id}" >/dev/null
for _ in {1..50}; do
    if cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "shell-smoke-output"; then
        break
    fi
    sleep 0.1
done
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "permission resolved"
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "shell.run"
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "shell-smoke-output"

cargo run --quiet -p bcode -- server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-shell-tool: PASS"
