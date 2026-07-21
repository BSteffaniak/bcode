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

[[event_subscriptions]]
topic = "example.event"

[[event_subscriptions]]
topic = "bcode.session.event"

[runtime]
type = "native"
abi_version = 2
library = "${dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF
cp "${plugin_dir}/bcode-plugin.toml" "${daemon_plugin_dir}/bcode-plugin.toml"

export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
disabled = ["example.hello"]
EOF

if "${root}/target/debug/bcode" plugin list --root "${workdir}/plugins" | grep "example.hello" >/dev/null; then
    echo "disabled plugin should not be listed" >&2
    exit 1
fi

cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["example.hello"]
EOF

"${root}/target/debug/bcode" plugin list --root "${workdir}/plugins" | grep "example.hello" >/dev/null
"${root}/target/debug/bcode" plugin services --root "${workdir}/plugins" | grep "example-hello/v1" >/dev/null
"${root}/target/debug/bcode" plugin check --root "${workdir}/plugins" | grep $'example.hello\tOK' >/dev/null
"${root}/target/debug/bcode" plugin invoke --root "${workdir}/plugins" example.hello example-hello/v1 echo "hello service" | grep "hello service" >/dev/null
"${root}/target/debug/bcode" plugin call --root "${workdir}/plugins" example-hello/v1 echo "hello routed service" | grep "hello routed service" >/dev/null
"${root}/target/debug/bcode" plugin publish --root "${workdir}/plugins" example.event "hello event" | grep $'delivered\t1' >/dev/null

export XDG_CONFIG_HOME="${workdir}/config"
mkdir -p "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_STATE_DIR="${workdir}/state"
"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..50}; do
    if "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
"${root}/target/debug/bcode" plugin services --daemon | grep "example-hello/v1" >/dev/null
"${root}/target/debug/bcode" plugin invoke --daemon example.hello example-hello/v1 echo "hello daemon service" | grep "hello daemon service" >/dev/null
"${root}/target/debug/bcode" plugin call --daemon example-hello/v1 echo "hello daemon routed service" | grep "hello daemon routed service" >/dev/null
session_id="$("${root}/target/debug/bcode" session create plugin-event-smoke)"
"${root}/target/debug/bcode" send "${session_id}" "plugin event smoke" >/dev/null
session_event_count="$("${root}/target/debug/bcode" plugin call --daemon example-hello/v1 event-count)"
if ! [[ "${session_event_count}" =~ ^[0-9]+$ ]] || (( session_event_count < 2 )); then
    echo "session create/send did not publish the expected plugin events: ${session_event_count}" >&2
    exit 1
fi
"${root}/target/debug/bcode" plugin publish --daemon example.event "daemon event" | grep $'delivered\t1' >/dev/null
final_event_count="$("${root}/target/debug/bcode" plugin call --daemon example-hello/v1 event-count)"
if ! [[ "${final_event_count}" =~ ^[0-9]+$ ]] || (( final_event_count <= session_event_count )); then
    echo "explicit plugin event did not increase the event count: ${session_event_count} -> ${final_event_count}" >&2
    exit 1
fi
"${root}/target/debug/bcode" server stop >/dev/null
for _ in {1..100}; do
    if ! kill -0 "${server_pid}" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if kill -0 "${server_pid}" 2>/dev/null; then
    echo "server did not stop cleanly" >&2
    exit 1
fi
wait "${server_pid}"
server_pid=""

echo "smoke-native-plugin: PASS"
