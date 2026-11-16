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

cargo build --quiet -p bcode --features distribution

export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
mkdir -p "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_STATE_DIR="${workdir}/state"
cat >"${BCODE_CONFIG}" <<EOF
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

# The fake provider does not implement prompt caching.
[model.prompt_cache]
mode = "off"
EOF

"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

session_id="$("${root}/target/debug/bcode" session create permission-smoke)"
target_file="${workdir}/written.txt"
"${root}/target/debug/bcode" send "${session_id}" "tool-write ${target_file} approved-content" >/dev/null
permission_id=""
for _ in {1..50}; do
    permission_id="$("${root}/target/debug/bcode" permission list | awk 'NR == 1 { print $1 }')"
    if [[ -n "${permission_id}" ]]; then
        break
    fi
    sleep 0.1
done
if [[ -z "${permission_id}" ]]; then
    echo "permission request was not recorded" >&2
    "${root}/target/debug/bcode" session history "${session_id}" >&2 || true
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi
"${root}/target/debug/bcode" permission approve "${permission_id}" >/dev/null
for _ in {1..100}; do
    if [[ -f "${target_file}" ]] && grep -q "approved-content" "${target_file}" \
        && "${root}/target/debug/bcode" session history "${session_id}" | grep "assistant [^:]*: fake tool result: .*wrote" >/dev/null; then
        break
    fi
    sleep 0.1
done
grep -q "approved-content" "${target_file}"
"${root}/target/debug/bcode" session history "${session_id}" | grep "permission resolved" >/dev/null
"${root}/target/debug/bcode" session history "${session_id}" | grep "assistant [^:]*: fake tool result: .*wrote" >/dev/null

"${root}/target/debug/bcode" server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-permission-checkpoint: PASS"
