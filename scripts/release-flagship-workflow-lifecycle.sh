#!/usr/bin/env bash
set -euo pipefail

# This proof uses the release binary and isolated daemon state. It deliberately keeps model-owned
# implementation nodes out of scope: release execution of those nodes needs a provider fixture,
# while this proof establishes that all three shipped documents are discoverable, their exact
# transitive child definitions are admitted, and durable public inspection survives restart.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-flagship-lifecycle.XXXXXX)"
mkdir -p "${workdir}/config" "${workdir}/repo" "${workdir}/state" "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
export XDG_CONFIG_HOME="${workdir}/config"

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
cargo build --quiet --release -p bcode --features distribution
bcode="${root}/target/release/bcode"

start_server() {
    local log="$1"
    rm -f "${BCODE_SOCKET}"
    "${bcode}" server run >"${log}" 2>&1 &
    server_pid="$!"
    for _ in {1..300}; do
        if [[ -S "${BCODE_SOCKET}" ]]; then
            return
        fi
        if ! kill -0 "${server_pid}" 2>/dev/null; then
            cat "${log}" >&2 || true
            exit 1
        fi
        sleep 0.1
    done
    cat "${log}" >&2 || true
    exit 1
}

start_server "${workdir}/before.log"
first_pid="${server_pid}"
session_id="$(cd "${workdir}/repo" && "${bcode}" session create flagship-release-proof)"

"${bcode}" workflow template-describe --owner bcode.workflow --template implementation-batch --version 1 --session "${session_id}" >"${workdir}/batch.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template delivery-tranche --version 1 --session "${session_id}" >"${workdir}/tranche.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template progress-driven-delivery --version 1 --session "${session_id}" >"${workdir}/parent.json"

kill "${server_pid}"
wait "${server_pid}" || true
server_pid=""
start_server "${workdir}/after.log"
second_pid="${server_pid}"

"${bcode}" workflow template-describe --owner bcode.workflow --template implementation-batch --version 1 --session "${session_id}" >"${workdir}/batch-after.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template delivery-tranche --version 1 --session "${session_id}" >"${workdir}/tranche-after.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template progress-driven-delivery --version 1 --session "${session_id}" >"${workdir}/parent-after.json"

python3 - "${workdir}" "${first_pid}" "${second_pid}" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
assert sys.argv[2] != sys.argv[3]
for name, template_id in (
    ("batch", "implementation-batch"),
    ("tranche", "delivery-tranche"),
    ("parent", "progress-driven-delivery"),
):
    before = json.loads(root.joinpath(f"{name}.json").read_text().split("\n", 1)[1])
    after = json.loads(root.joinpath(f"{name}-after.json").read_text().split("\n", 1)[1])
    assert before == after
    encoded = json.dumps(before)
    assert template_id in encoded
    assert "diagnostics" in encoded
PY

"${bcode}" server stop >/dev/null
wait "${server_pid}"
server_pid=""
echo "release-flagship-workflow-lifecycle: PASS"
