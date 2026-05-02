#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
export BCODE_SOCKET="$workdir/bcode.sock"
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

cargo run --quiet -p bcode -- server start >"$workdir/server.log" 2>&1 &
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

session_id="$(cargo run --quiet -p bcode -- session create smoke)"
cargo run --quiet -p bcode -- server status
cargo run --quiet -p bcode -- session list

cargo run --quiet -p bcode -- attach "${session_id}" >"$workdir/attach.log" 2>&1 &
attach_pid="$!"
sleep 0.5

cargo run --quiet -p bcode -- send "${session_id}" "hello from smoke"
sleep 0.5

if ! grep -q "hello from smoke" "$workdir/attach.log"; then
    echo "attached client did not receive sent message" >&2
    echo "--- attach log ---" >&2
    cat "$workdir/attach.log" >&2 || true
    echo "--- server log ---" >&2
    cat "$workdir/server.log" >&2 || true
    exit 1
fi

cargo run --quiet -p bcode -- server stop
wait "${server_pid}"
server_pid=""

cargo run --quiet -p bcode -- server start >"$workdir/server-restarted.log" 2>&1 &
server_pid="$!"
for _ in {1..300}; do
    if cargo run --quiet -p bcode -- session list | grep -q "${session_id}"; then
        break
    fi
    sleep 0.1
done

if ! cargo run --quiet -p bcode -- session list | grep -q "${session_id}"; then
    echo "persisted session was not restored after server restart" >&2
    echo "--- restarted server log ---" >&2
    cat "$workdir/server-restarted.log" >&2 || true
    exit 1
fi

if ! cargo run --quiet -p bcode -- session history "${session_id}" | grep -q "hello from smoke"; then
    echo "persisted session history did not include sent message" >&2
    echo "--- restarted server log ---" >&2
    cat "$workdir/server-restarted.log" >&2 || true
    exit 1
fi

cargo run --quiet -p bcode -- server stop
wait "${server_pid}"
server_pid=""

echo "smoke-local-daemon: PASS"
