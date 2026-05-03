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

[permissions]
allow_shell_command_prefixes = ["printf policy-allowed"]
deny_shell_command_prefixes = ["printf policy-denied"]
EOF

cargo run --quiet -p bcode -- server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if cargo run --quiet -p bcode -- server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

session_id="$(cargo run --quiet -p bcode -- session create permission-policy-smoke)"
cargo run --quiet -p bcode -- send "${session_id}" "tool-shell printf policy-allowed-shell" >/dev/null
for _ in {1..50}; do
    if cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "policy-allowed-shell"; then
        break
    fi
    sleep 0.1
done
if [[ -n "$(cargo run --quiet -p bcode -- permission list)" ]]; then
    echo "allow_tools policy should not leave a pending permission" >&2
    cargo run --quiet -p bcode -- permission list >&2 || true
    exit 1
fi
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "permission requested"
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "permission resolved: .*approved=true"
cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "policy-allowed-shell"

blocked_session_id="$(cargo run --quiet -p bcode -- session create permission-policy-deny-smoke)"
cargo run --quiet -p bcode -- send "${blocked_session_id}" "tool-shell printf policy-denied-shell" >/dev/null
for _ in {1..50}; do
    if cargo run --quiet -p bcode -- session history "${blocked_session_id}" | grep -q "permission denied"; then
        break
    fi
    sleep 0.1
done
if [[ -n "$(cargo run --quiet -p bcode -- permission list)" ]]; then
    echo "deny policy should not leave a pending permission" >&2
    cargo run --quiet -p bcode -- permission list >&2 || true
    exit 1
fi
cargo run --quiet -p bcode -- session history "${blocked_session_id}" | grep -q "permission requested"
cargo run --quiet -p bcode -- session history "${blocked_session_id}" | grep -q "permission resolved: .*approved=false"
cargo run --quiet -p bcode -- session history "${blocked_session_id}" | grep -q "permission denied"

cargo run --quiet -p bcode -- permission allow-shell-prefix "printf cli-added" | grep -q "permission rule added"
grep -q "printf cli-added" "${BCODE_CONFIG}"
cli_rule_session_id="$(cargo run --quiet -p bcode -- session create permission-policy-cli-rule-smoke)"
cargo run --quiet -p bcode -- send "${cli_rule_session_id}" "tool-shell printf cli-added-rule" >/dev/null
for _ in {1..50}; do
    if cargo run --quiet -p bcode -- session history "${cli_rule_session_id}" | grep -q "cli-added-rule"; then
        break
    fi
    sleep 0.1
done
cargo run --quiet -p bcode -- session history "${cli_rule_session_id}" | grep -q "permission resolved: .*approved=true"
cargo run --quiet -p bcode -- session history "${cli_rule_session_id}" | grep -q "cli-added-rule"

cargo run --quiet -p bcode -- server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-permission-policy: PASS"
