#!/usr/bin/env bash
set -euo pipefail

# Release-binary proof for source-defined JSON/TOML authoring, exact shell-v2 dispatch,
# generic typed routing, restart-safe waits, and authoritative terminal inspection.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-source-workflow.XXXXXX)"
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
        [[ -S "${BCODE_SOCKET}" ]] && return
        if ! kill -0 "${server_pid}" 2>/dev/null; then
            cat "${log}" >&2 || true
            exit 1
        fi
        sleep 0.1
    done
    echo "source workflow daemon did not become ready" >&2
    exit 1
}

plugin_json() {
    python3 - "$1" <<'PY'
import json, pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
print(json.dumps(json.loads(text.split("\n", 1)[1])))
PY
}

wait_for_approval() {
    local run_id="$1" output="$2"
    for _ in {1..300}; do
        "${bcode}" workflow-ui inspect --id "${run_id}" --session "${session_id}" >"${output}" || true
        if [[ -s "${output}" ]] && python3 - "${output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
raise SystemExit(0 if value[0]["options"].get("mutation_approvals") else 1)
PY
        then return; fi
        sleep 0.1
    done
    echo "source workflow mutation approval did not appear" >&2
    exit 1
}

wait_for_input() {
    local run_id="$1" node_id="$2" output="$3"
    for _ in {1..300}; do
        "${bcode}" workflow-ui inspect --id "${run_id}" --session "${session_id}" >"${output}" || true
        if [[ -s "${output}" ]] && python3 - "${output}" "${node_id}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
waits=value[0]["options"].get("waits",[])
raise SystemExit(0 if any(wait["node_id"] == sys.argv[2] for wait in waits) else 1)
PY
        then return; fi
        sleep 0.1
    done
    echo "source workflow route wait ${node_id} did not appear" >&2
    exit 1
}

resolve_run() {
    local run_id="$1" expected_node="$2" prefix="$3"
    wait_for_approval "${run_id}" "${workdir}/${prefix}-approval.txt"
    read -r approval_id < <(python3 - "${workdir}/${prefix}-approval.txt" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
print(value[0]["options"]["mutation_approvals"][0]["approval_id"])
PY
)
    "${bcode}" workflow-ui approve-mutation --id "${run_id}" --approval "${approval_id}" \
        --session "${session_id}" >"${workdir}/${prefix}-approved.json"
    wait_for_input "${run_id}" "${expected_node}" "${workdir}/${prefix}-wait.txt"
}

start_server "${workdir}/server-before.log"
session_id="$(cd "${workdir}/repo" && "${bcode}" session create source-workflow-proof)"

"${bcode}" workflow author validate \
    "${root}/fixtures/workflows/source-defined-input.workflow.json" >"${workdir}/json-validation.json"
"${bcode}" workflow author validate \
    "${root}/fixtures/workflows/source-defined-input.workflow.toml" >"${workdir}/toml-validation.json"
"${bcode}" workflow author preview \
    "${root}/fixtures/workflows/shell-v2-exit-routing.workflow.json" >"${workdir}/shell-preview.json"

# Prove both source formats enter the same durable draft lifecycle and stale replacement fails
# through the public optimistic-generation contract.
"${bcode}" workflow author create --draft-id portable-draft \
    "${root}/fixtures/workflows/source-defined-input.workflow.toml" \
    >"${workdir}/portable-created.json"
"${bcode}" workflow author update --workflow-id example/source-defined-input \
    --draft-id portable-draft --expected-generation 1 \
    "${root}/fixtures/workflows/source-defined-input.workflow.json" \
    >"${workdir}/portable-updated.json"
"${bcode}" workflow author update --workflow-id example/source-defined-input \
    --draft-id portable-draft --expected-generation 1 \
    "${root}/fixtures/workflows/source-defined-input.workflow.json" \
    >"${workdir}/portable-conflict.json"

python3 - "${root}/fixtures/workflows/shell-v2-exit-routing.workflow.json" \
    "${workdir}/accepted.json" "${workdir}/unaccepted.json" <<'PY'
import json, pathlib, sys
source=json.loads(pathlib.Path(sys.argv[1]).read_text())
accepted=source
accepted["workflow_id"]="release/source-shell-accepted"
accepted["metadata"]["title"]="Release source shell accepted"
pathlib.Path(sys.argv[2]).write_text(json.dumps(accepted, indent=2)+"\n")
unaccepted=json.loads(pathlib.Path(sys.argv[1]).read_text())
unaccepted["workflow_id"]="release/source-shell-unaccepted"
unaccepted["metadata"]["title"]="Release source shell unaccepted"
unaccepted["configuration_defaults"]["command_plan"]["commands"][0]["accepted_exit_codes"]=[0]
pathlib.Path(sys.argv[3]).write_text(json.dumps(unaccepted, indent=2)+"\n")
PY

for outcome in accepted unaccepted; do
    "${bcode}" workflow author create --draft-id source-draft "${workdir}/${outcome}.json" \
        >"${workdir}/${outcome}-created.json"
    "${bcode}" workflow author publish \
        --workflow-id "release/source-shell-${outcome}" --draft-id source-draft \
        --expected-generation 1 --activate >"${workdir}/${outcome}-published.json"
    "${bcode}" workflow author fork \
        --workflow-id "release/source-shell-${outcome}" --draft-id durable-draft \
        --source-revision 1 >"${workdir}/${outcome}-durable-draft.json"
    "${bcode}" workflow start --parent-session-id "${session_id}" \
        --run-id "source-${outcome}-run" active \
        --workflow-id "release/source-shell-${outcome}" >"${workdir}/${outcome}-started.json"
done

resolve_run source-accepted-run accepted accepted

# Restart at the actual typed accepted-route wait and prove durable draft/revision/run recovery.
"${bcode}" server stop >/dev/null
wait "${server_pid}"
server_pid=""
start_server "${workdir}/server-after.log"
"${bcode}" workflow author draft get --workflow-id release/source-shell-accepted \
    --draft-id durable-draft >"${workdir}/accepted-draft-after-restart.json"
"${bcode}" workflow author revision get --workflow-id release/source-shell-accepted \
    --revision 1 >"${workdir}/accepted-revision-after-restart.json"
wait_for_input source-accepted-run accepted "${workdir}/accepted-wait-after-restart.txt"

resolve_run source-unaccepted-run unaccepted unaccepted

for outcome in accepted unaccepted; do
    wait_file="${workdir}/${outcome}-wait.txt"
    [[ "${outcome}" == accepted ]] && wait_file="${workdir}/accepted-wait-after-restart.txt"
    read -r node_id activation_id value < <(python3 - "${wait_file}" <<'PY'
import json, pathlib, shlex, sys
root=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])[0]["options"]
wait=root["waits"][0]
print(wait["node_id"], wait["activation_id"], shlex.quote(json.dumps(wait["input"],separators=(",",":"))))
PY
)
    eval "typed_value=${value}"
    "${bcode}" workflow-ui provide-input --id "source-${outcome}-run" --node "${node_id}" \
        --activation "${activation_id}" --input "${typed_value}" --session "${session_id}" \
        >"${workdir}/${outcome}-resolved.json"
    "${bcode}" workflow-ui inspect --id "source-${outcome}-run" --session "${session_id}" \
        >"${workdir}/${outcome}-terminal.txt"
done

python3 - "${workdir}" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
json_validation=json.loads((root/"json-validation.json").read_text())
toml_validation=json.loads((root/"toml-validation.json").read_text())
assert json_validation["lowering"]["validation"]["valid"] and toml_validation["lowering"]["validation"]["valid"]
assert json_validation["lowering"]["validation"]["source_digest_sha256"] == toml_validation["lowering"]["validation"]["source_digest_sha256"]
assert json_validation["lowering"]["validation"]["executable_source_digest_sha256"] == toml_validation["lowering"]["validation"]["executable_source_digest_sha256"]
preview=json.loads((root/"shell-preview.json").read_text())["preview"]
assert preview["compiled"]["requirements"]["blocks"] == ["bcode.shell/shell.command-plan@2"]
portable_created=json.loads((root/"portable-created.json").read_text())
portable_updated=json.loads((root/"portable-updated.json").read_text())
portable_conflict=json.loads((root/"portable-conflict.json").read_text())
assert portable_created[1]["generation"] == 1
assert portable_updated["updated"]["generation"] == 2
assert portable_conflict["conflict"]["expected_generation"] == 1
assert portable_conflict["conflict"]["current_generation"] == 2
for outcome, expected in (("accepted", True), ("unaccepted", False)):
    value=json.loads((root/f"{outcome}-terminal.txt").read_text().split("\n",1)[1])[0]["options"]
    assert value["run"]["status"] == "completed"
    terminal=value["terminal_output"]
    assert terminal["node_id"] == outcome
    assert terminal["value"]["commands"][0]["exit_code"] == 7
    assert terminal["value"]["commands"][0]["exit_accepted"] is expected
assert json.loads((root/"accepted-draft-after-restart.json").read_text())["identity"]["draft_id"] == "durable-draft"
assert json.loads((root/"accepted-revision-after-restart.json").read_text())["identity"]["revision"] == 1
PY

"${bcode}" server stop >/dev/null
wait "${server_pid}"
server_pid=""
echo "release-source-defined-workflow-lifecycle: PASS"
