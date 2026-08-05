#!/usr/bin/env bash
set -euo pipefail

# Release-binary proof for concise source validation, canonical lowering, apply, publication,
# default shell execution, and equivalent JSON/YAML/TOML semantics.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-usable-source-workflow.XXXXXX)"
mkdir -p "${workdir}/config" "${workdir}/state" "${workdir}/repo" "${workdir}/tmp"
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
        kill -0 "${server_pid}" 2>/dev/null || { cat "${log}" >&2; exit 1; }
        sleep 0.1
    done
    echo "usable-source daemon did not become ready" >&2
    exit 1
}

start_server "${workdir}/server.log"
session_id="$(cd "${workdir}/repo" && "${bcode}" session create usable-source-proof)"

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
    echo "usable-source mutation approval did not appear for ${run_id}" >&2
    exit 1
}

approve_next() {
    local run_id="$1" prefix="$2"
    wait_for_approval "${run_id}" "${workdir}/${prefix}-approval.txt"
    local approval_id
    approval_id="$(python3 - "${workdir}/${prefix}-approval.txt" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
print(value[0]["options"]["mutation_approvals"][0]["approval_id"])
PY
)"
    "${bcode}" workflow-ui approve-mutation --id "${run_id}" --approval "${approval_id}" \
        --session "${session_id}" >"${workdir}/${prefix}-approved.json"
}

wait_for_terminal() {
    local run_id="$1" output="$2"
    for _ in {1..300}; do
        "${bcode}" workflow-ui inspect --id "${run_id}" --session "${session_id}" >"${output}" || true
        if [[ -s "${output}" ]] && python3 - "${output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])[0]["options"]
raise SystemExit(0 if value.get("run",{}).get("status") in ("completed","failed","cancelled") else 1)
PY
        then return; fi
        sleep 0.1
    done
    echo "usable-source run ${run_id} did not terminate" >&2
    exit 1
}

for format in json yaml toml; do
    "${bcode}" workflow author validate \
        "${root}/fixtures/workflows/concise-run.workflow.${format}" \
        >"${workdir}/${format}-validation.json"
    "${bcode}" workflow author preview \
        "${root}/fixtures/workflows/concise-run.workflow.${format}" \
        >"${workdir}/${format}-preview.json"
done
"${bcode}" workflow author apply \
    "${root}/fixtures/workflows/concise-run.workflow.yaml" >"${workdir}/applied.json"
"${bcode}" workflow author apply \
    "${root}/fixtures/workflows/concise-run.workflow.json" >"${workdir}/updated.json"
"${bcode}" workflow author publish --workflow-id example/concise-run --draft-id source \
    --expected-generation 2 --activate >"${workdir}/published.json"
"${bcode}" workflow author fork --workflow-id example/concise-run --draft-id durable-source \
    --source-revision 1 >"${workdir}/durable-draft.json"

# Prove a generic exact `uses` source enters the same canonical lifecycle.
"${bcode}" workflow author validate \
    "${root}/fixtures/workflows/concise-uses.workflow.json" >"${workdir}/uses-validation.json"
"${bcode}" workflow author apply \
    "${root}/fixtures/workflows/concise-uses.workflow.json" >"${workdir}/uses-applied.json"

# Restart and inspect authoritative canonical source state.
"${bcode}" server stop >/dev/null
wait "${server_pid}"
server_pid=""
start_server "${workdir}/server-after.log"
"${bcode}" workflow author draft get --workflow-id example/concise-run \
    --draft-id durable-source >"${workdir}/draft-after-restart.json"
"${bcode}" workflow author revision get --workflow-id example/concise-run \
    --revision 1 >"${workdir}/revision-after-restart.json"
"${bcode}" workflow start --parent-session-id "${session_id}" --run-id concise-run active \
    --workflow-id example/concise-run >"${workdir}/started.json"
approve_next concise-run first
approve_next concise-run second
wait_for_terminal concise-run "${workdir}/terminal.txt"

# Prove accepted and unaccepted nonzero concise routing with shell-plugin-owned policy.
for kind in accepted-nonzero unaccepted; do
    "${bcode}" workflow author apply \
        "${root}/fixtures/workflows/concise-${kind}.workflow.yaml" >"${workdir}/${kind}-applied.json"
    "${bcode}" workflow author publish --workflow-id "example/concise-${kind}" --draft-id source \
        --expected-generation 1 --activate >"${workdir}/${kind}-published.json"
    "${bcode}" workflow start --parent-session-id "${session_id}" --run-id "concise-${kind}" active \
        --workflow-id "example/concise-${kind}" >"${workdir}/${kind}-started.json"
done
approve_next concise-accepted-nonzero accepted-nonzero-first
approve_next concise-accepted-nonzero accepted-nonzero-second
wait_for_terminal concise-accepted-nonzero "${workdir}/accepted-nonzero-terminal.txt"
approve_next concise-unaccepted unaccepted-first
wait_for_terminal concise-unaccepted "${workdir}/unaccepted-terminal.txt"

python3 - "${workdir}" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
validations=[json.loads((root/f"{fmt}-validation.json").read_text()) for fmt in ("json","yaml","toml")]
for value in validations:
    assert value["lowering"]["validation"]["valid"]
documents=[value["lowering"]["document"] for value in validations]
assert documents[0] == documents[1] == documents[2]
previews=[json.loads((root/f"{fmt}-preview.json").read_text()) for fmt in ("json","yaml","toml")]
digests=[value["lowering"]["validation"]["source_digest_sha256"] for value in previews]
assert digests[0] == digests[1] == digests[2]
applied=json.loads((root/"applied.json").read_text())
updated=json.loads((root/"updated.json").read_text())
assert applied["outcome"] == "created"
assert updated["outcome"] == "updated"
assert applied["draft_id"] == "source" and updated["generation"] == 2
assert applied["requirements"]["blocks"] == ["bcode.shell/shell.script@1"]
assert applied["effects"]["block_effects"] == ["mutating"]
assert len(applied["source_map"]["entries"]) == 2
published=json.loads((root/"published.json").read_text())
assert published["published"]["revision"]["identity"]["revision"] == 1
uses=json.loads((root/"uses-validation.json").read_text())
assert uses["lowering"]["validation"]["valid"]
draft=json.loads((root/"draft-after-restart.json").read_text())
revision=json.loads((root/"revision-after-restart.json").read_text())
assert draft["identity"]["draft_id"] == "durable-source"
assert revision["identity"]["revision"] == 1
terminal=json.loads((root/"terminal.txt").read_text().split("\n",1)[1])[0]["options"]
assert terminal["run"]["status"] == "completed"
assert terminal["terminal_output"]["node_id"] == "second"
assert terminal["terminal_output"]["value"]["passed"] is True
accepted=json.loads((root/"accepted-nonzero-terminal.txt").read_text().split("\n",1)[1])[0]["options"]
unaccepted=json.loads((root/"unaccepted-terminal.txt").read_text().split("\n",1)[1])[0]["options"]
assert accepted["run"]["status"] == "completed"
assert accepted["terminal_output"]["node_id"] == "after_accepted"
assert unaccepted["run"]["status"] == "failed"
assert "must_not_run" not in unaccepted.get("outputs", {})
PY

"${bcode}" server stop >/dev/null
wait "${server_pid}"
server_pid=""
echo "release-usable-source-workflow-lifecycle: PASS"
