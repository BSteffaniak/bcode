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
enabled = ["bcode.fake-provider", "bcode.shell"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[agent.build.permission]
command = { "*" = "ask", "printf policy-allowed*" = "allow", "printf policy-denied*" = "deny" }

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

session_id="$("${root}/target/debug/bcode" session create permission-policy-smoke)"
"${root}/target/debug/bcode" send "${session_id}" "tool-shell printf policy-allowed-shell" >/dev/null
for _ in {1..100}; do
    if "${root}/target/debug/bcode" session history "${session_id}" | grep "assistant [^:]*: fake tool result: .*policy-allowed-shell" >/dev/null; then
        break
    fi
    sleep 0.1
done
if [[ -n "$("${root}/target/debug/bcode" permission list)" ]]; then
    echo "allow_tools policy should not leave a pending permission" >&2
    "${root}/target/debug/bcode" permission list >&2 || true
    exit 1
fi
"${root}/target/debug/bcode" session history "${session_id}" | grep "assistant [^:]*: fake tool result: .*policy-allowed-shell" >/dev/null

blocked_session_id="$("${root}/target/debug/bcode" session create permission-policy-deny-smoke)"
"${root}/target/debug/bcode" send "${blocked_session_id}" "tool-shell printf policy-denied-shell" >/dev/null
for _ in {1..100}; do
    if "${root}/target/debug/bcode" session history "${blocked_session_id}" | grep "invocation result (error): .*denied shell program: .*policy-denied-shell" >/dev/null; then
        break
    fi
    sleep 0.1
done
if [[ -n "$("${root}/target/debug/bcode" permission list)" ]]; then
    echo "deny policy should not leave a pending permission" >&2
    "${root}/target/debug/bcode" permission list >&2 || true
    exit 1
fi
"${root}/target/debug/bcode" session history "${blocked_session_id}" | grep "invocation result (error): .*denied shell program: .*policy-denied-shell" >/dev/null

"${root}/target/debug/bcode" permission add --agent build --category command --pattern "printf cli-added*" --action allow | grep "permission rule added" >/dev/null
grep -q "printf cli-added" "${BCODE_STATE_DIR}/permissions.toml"
cli_rule_session_id="$("${root}/target/debug/bcode" session create permission-policy-cli-rule-smoke)"
"${root}/target/debug/bcode" send "${cli_rule_session_id}" "tool-shell printf cli-added-rule" >/dev/null
for _ in {1..100}; do
    if "${root}/target/debug/bcode" session history "${cli_rule_session_id}" | grep "assistant [^:]*: fake tool result: .*cli-added-rule" >/dev/null; then
        break
    fi
    sleep 0.1
done
"${root}/target/debug/bcode" session history "${cli_rule_session_id}" | grep "assistant [^:]*: fake tool result: .*cli-added-rule" >/dev/null

"${root}/target/debug/bcode" server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-permission-policy: PASS"
