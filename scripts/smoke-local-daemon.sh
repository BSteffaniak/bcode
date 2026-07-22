#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-smoke.XXXXXX)"
mkdir -p "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="$workdir/state"

server_pid=""
attach_pid=""
cleanup() {
    if [[ -n "${attach_pid}" ]] && kill -0 "${attach_pid}" 2>/dev/null; then
        kill "${attach_pid}" 2>/dev/null || true
        wait "${attach_pid}" 2>/dev/null || true
    fi
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"

cargo build --quiet -p bcode --features app

"${root}/target/debug/bcode" server run >"$workdir/server.log" 2>&1 &
server_pid="$!"

for _ in {1..300}; do
    if [[ -S "${BCODE_SOCKET}" ]]; then
        break
    fi
    sleep 0.1
done

if [[ ! -S "${BCODE_SOCKET}" ]]; then
    echo "server socket was not created" >&2
    cat "$workdir/server.log" >&2 || true
    exit 1
fi

session_id="$("${root}/target/debug/bcode" session create smoke)"
"${root}/target/debug/bcode" server status
"${root}/target/debug/bcode" session list

"${root}/target/debug/bcode" attach "${session_id}" >"$workdir/attach.log" 2>&1 &
attach_pid="$!"
sleep 0.5

"${root}/target/debug/bcode" send "${session_id}" "hello from smoke"
sleep 0.5

if ! grep -q "hello from smoke" "$workdir/attach.log"; then
    echo "attached client did not receive sent message" >&2
    echo "--- attach log ---" >&2
    cat "$workdir/attach.log" >&2 || true
    echo "--- server log ---" >&2
    cat "$workdir/server.log" >&2 || true
    exit 1
fi

"${root}/target/debug/bcode" server stop
wait "${server_pid}"
server_pid=""

"${root}/target/debug/bcode" server run >"$workdir/server-restarted.log" 2>&1 &
server_pid="$!"
for _ in {1..300}; do
    if "${root}/target/debug/bcode" session list 2>/dev/null | grep "${session_id}" >/dev/null; then
        break
    fi
    sleep 0.1
done

if ! "${root}/target/debug/bcode" session list 2>/dev/null | grep "${session_id}" >/dev/null; then
    echo "persisted session was not restored after server restart" >&2
    echo "--- restarted server log ---" >&2
    cat "$workdir/server-restarted.log" >&2 || true
    exit 1
fi

if ! "${root}/target/debug/bcode" session history "${session_id}" | grep "hello from smoke" >/dev/null; then
    echo "persisted session history did not include sent message" >&2
    echo "--- restarted server log ---" >&2
    cat "$workdir/server-restarted.log" >&2 || true
    exit 1
fi

"${root}/target/debug/bcode" server stop
wait "${server_pid}"
server_pid=""

"${root}/target/debug/bcode" server start >/dev/null
status_output="$("${root}/target/debug/bcode" server status --verbose)"
client_digest="$(printf '%s\n' "${status_output}" | awk '/^client executable identity:/ {print $4}')"
daemon_digest="$(printf '%s\n' "${status_output}" | awk '/^executable identity:/ {print $3}')"
daemon_executable="$(printf '%s\n' "${status_output}" | sed -n 's/^daemon executable: //p')"
if [[ -z "${client_digest}" || "${client_digest}" != "${daemon_digest}" ]]; then
    echo "detached daemon executable identity does not match client" >&2
    printf '%s\n' "${status_output}" >&2
    exit 1
fi
if [[ ! "${daemon_executable}" =~ /daemon-images/.*/${daemon_digest}/bcode(.exe)?$ ]]; then
    echo "detached daemon did not start from a content-addressed image" >&2
    printf '%s\n' "${status_output}" >&2
    exit 1
fi
if [[ ! -x "${daemon_executable}" ]]; then
    echo "content-addressed daemon image is not executable" >&2
    exit 1
fi
cached_digest="$(shasum -a 256 "${daemon_executable}" | awk '{print $1}')"
if [[ "${cached_digest}" != "${daemon_digest}" ]]; then
    echo "content-addressed daemon image failed digest verification" >&2
    exit 1
fi
"${root}/target/debug/bcode" server stop >/dev/null
sleep 1

"${root}/target/debug/bcode" server start >/dev/null
restarted_executable="$("${root}/target/debug/bcode" server status --verbose | sed -n 's/^daemon executable: //p')"
if [[ "${restarted_executable}" != "${daemon_executable}" ]]; then
    echo "daemon restart did not reuse the immutable image" >&2
    exit 1
fi
"${root}/target/debug/bcode" server stop >/dev/null

echo "smoke-local-daemon: PASS"
