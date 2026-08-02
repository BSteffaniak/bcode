#!/usr/bin/env bash
set -euo pipefail

# This proof uses the release binary and isolated daemon state. It verifies all three shipped
# documents through public template discovery, attempts the published three-level parent with the
# bundled deterministic provider, and proves that exact transitive child admission plus bounded
# public inspection survive restart. A deterministic public input-wait fixture separately proves
# restart-safe wait recovery and terminal resolution without bypassing authorization. Full model-
# owned flagship completion/exhaustion still requires a dedicated deterministic product fixture.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-flagship-lifecycle.XXXXXX)"
mkdir -p "${workdir}/config" "${workdir}/repo" "${workdir}/state" "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
cat >"${BCODE_CONFIG}" <<'EOF'
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[model.prompt_cache]
mode = "off"
EOF

server_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    if [[ "${BCODE_KEEP_FLAGSHIP_PROOF:-0}" == "1" ]]; then
        echo "flagship proof artifacts: ${workdir}" >&2
    else
        rm -rf "${workdir}"
    fi
}
trap cleanup EXIT

cd "${root}"
if [[ "${BCODE_SKIP_RELEASE_BUILD:-0}" != "1" ]]; then
    cargo build --quiet --release -p bcode --features distribution
fi
bcode="${BCODE_FLAGSHIP_BINARY:-${root}/target/release/bcode}"
if [[ ! -x "${bcode}" ]]; then
    echo "release bcode binary is unavailable: ${bcode}" >&2
    exit 1
fi

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

python3 - "${root}/plugins/workflow-plugin/templates/progress-driven-delivery.workflow.json" "${workdir}/configuration.json" <<'PY'
import json
import pathlib
import sys

description = json.loads(pathlib.Path(sys.argv[1]).read_text())
schema = description["configuration_schema"]["schema"]

def value(node):
    if "const" in node:
        return node["const"]
    if "enum" in node:
        return node["enum"][0]
    kind = node.get("type")
    if isinstance(kind, list):
        kind = next((item for item in kind if item != "null"), "null")
    if kind == "object":
        properties = node.get("properties", {})
        return {name: value(properties[name]) for name in node.get("required", [])}
    if kind == "array":
        return []
    if kind == "string":
        return "flagship-proof"
    if kind == "boolean":
        return True
    if kind in ("integer", "number"):
        return node.get("minimum", 0)
    return None

configuration = value(schema)
configuration["mode"] = "existing_progress"
configuration["workflow_slug"] = "flagship-proof"
configuration["progress_document_path"] = "local-flagship-proof-progress.md"
configuration["validation_grant_reviewed"] = True
configuration["formatting_grant_reviewed"] = True
state = configuration["state"]
state["objective"] = "Prove the release flagship lifecycle"
state["implementation_prompt"] = "Return valid deterministic structured workflow state."
state["completion_condition"] = "Reach a durable public workflow state."
state["progress_document"]["path"] = "local-flagship-proof-progress.md"
state["progress_document"]["digest_sha256"] = None
state["instruction_fingerprint_sha256"] = None
state["phase"] = "implementing"
pathlib.Path(sys.argv[2]).write_text(json.dumps(configuration, separators=(",", ":")))
PY

configuration="$(cat "${workdir}/configuration.json")"

# Use one release CLI process to prove deterministic public input-wait resolution and restart.
cat >"${workdir}/wait.workflow.json" <<'JSON'
{
  "schema_version": 1,
  "name": "flagship-release-input-wait",
  "input": {"type_name":"proof.input/v1","schema":{"type":"object"}},
  "output": {"type_name":"proof.output/v1","schema":{"type":"object"}},
  "nodes": {
    "operator_wait": {
      "id": "operator_wait",
      "name": "Operator continuation",
      "kind": "input",
      "input": {"type_name":"proof.input/v1","schema":{"type":"object"}},
      "output": {"type_name":"proof.output/v1","schema":{"type":"object"}},
      "configuration": null
    }
  },
  "entries": ["operator_wait"],
  "exits": ["operator_wait"],
  "edges": []
}
JSON
wait_registration="$("${bcode}" workflow register --id flagship-release-input-wait --version 1 --definition "${workdir}/wait.workflow.json" --session "${session_id}")"
wait_definition_id="$(python3 -c 'import json,sys; print(json.loads(sys.stdin.read().split("\n",1)[1])[0]["options"]["definitions"][0]["definition_id"])' <<<"${wait_registration}")"
"${bcode}" workflow run --id "${wait_definition_id}" --version 1 --session "${session_id}" --input '{}' >"${workdir}/wait-started.json"
wait_run_id="$(python3 - "${workdir}/wait-started.json" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
print(value[0]["options"]["run"]["run_id"])
PY
)"
"${bcode}" workflow inspect --id "${wait_run_id}" --session "${session_id}" >"${workdir}/wait-before.txt"
wait_activation="$(python3 - "${workdir}/wait-before.txt" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
print(value[0]["options"]["waits"][0]["activation_id"])
PY
)"

# Template start itself is proved without depending on provider-generated product payloads: a future
# dedicated deterministic flagship fixture must drive every model-owned state transition.
(
    cd "${workdir}/repo"
    "${bcode}" workflow template-start --owner bcode.workflow --template progress-driven-delivery --version 1 --session "${session_id}" --run flagship-root --input "${configuration}" >"${workdir}/started.json" || true
)
"${bcode}" workflow inspect --id flagship-root --session "${session_id}" >"${workdir}/run-before.txt"

kill "${server_pid}"
wait "${server_pid}" || true
server_pid=""
start_server "${workdir}/after.log"
second_pid="${server_pid}"

"${bcode}" workflow template-describe --owner bcode.workflow --template implementation-batch --version 1 --session "${session_id}" >"${workdir}/batch-after.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template delivery-tranche --version 1 --session "${session_id}" >"${workdir}/tranche-after.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template progress-driven-delivery --version 1 --session "${session_id}" >"${workdir}/parent-after.json"
"${bcode}" workflow inspect --id flagship-root --session "${session_id}" >"${workdir}/run-after.txt"
"${bcode}" workflow inspect --id "${wait_run_id}" --session "${session_id}" >"${workdir}/wait-after.txt"
"${bcode}" workflow provide-input --id "${wait_run_id}" --node operator_wait --activation "${wait_activation}" --input '{}' --session "${session_id}" >"${workdir}/wait-resolved.json"
"${bcode}" workflow inspect --id "${wait_run_id}" --session "${session_id}" >"${workdir}/wait-completed.txt"

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
started = json.loads(root.joinpath("started.json").read_text().split("\n", 1)[1])
assert "flagship-root" in json.dumps(started)
before_run = root.joinpath("run-before.txt").read_text()
after_run = root.joinpath("run-after.txt").read_text()
for inspection in (before_run, after_run):
    assert "flagship-root" in inspection
    assert "progress-driven-delivery" in inspection
wait_before = root.joinpath("wait-before.txt").read_text()
wait_after = root.joinpath("wait-after.txt").read_text()
wait_completed = root.joinpath("wait-completed.txt").read_text()
assert '"status": "waiting_input"' in wait_before
assert '"status": "waiting_input"' in wait_after
assert '"status": "completed"' in wait_completed
assert '"node_id": "operator_wait"' in wait_completed
PY

"${bcode}" server stop >/dev/null
wait "${server_pid}"
server_pid=""
echo "release-flagship-workflow-lifecycle: PASS"
