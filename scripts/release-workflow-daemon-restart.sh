#!/usr/bin/env bash
set -euo pipefail

# This proof owns isolated daemon state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-workflow-restart.XXXXXX)"
mkdir -p "${workdir}/config" "${workdir}/repo" "${workdir}/state" "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
active_loss="${BCODE_RESTART_ACTIVE_LOSS:-0}"
if [[ "${active_loss}" == "1" ]]; then
    export BCODE_FAKE_PROVIDER_DELAY_MS=10000
fi
cat >"${BCODE_CONFIG}" <<'TOML'
[workflows]
run_publication_local_clients = true

[model]
profile = "restart-proof"

[model.profiles.restart-proof]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[model.profiles.restart-proof.settings]
fake_structured_output_json = '{"recovered":true}'

[model.prompt_cache]
mode = "off"
TOML

server_pid=""
cleanup() {
    local result=$?
    if [[ "${result}" -ne 0 ]]; then
        cat "${workdir}"/server-*.log >&2 2>/dev/null || true
        if [[ -f "${workdir}/completed.json" ]]; then
            python3 - "${workdir}/completed.json" >&2 <<'PY'
import json
import sys
view = json.load(open(sys.argv[1]))
print(json.dumps({"status": view["run"]["status"], "attempts": view["attempts"]}))
PY
        fi
    fi
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"
cargo build --quiet --release -p bcode --features app,static-bundled-plugins
bcode="${root}/target/release/bcode"

cat >"${workdir}/workflow.json" <<'JSON'
{
  "schema_version": 2,
  "workflow_id": "release/restart-proof",
  "metadata": {
    "title": "Release restart proof",
    "description": "Durable authored state process restart acceptance",
    "labels": {}
  },
  "configuration_schema": {
    "type_name": "release.restart.config/v1",
    "schema": {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "type": "object",
      "additionalProperties": false
    }
  },
  "configuration_defaults": {},
  "definition": {
    "schema_version": 2,
    "name": "release-restart-proof",
    "input": {
      "type_name": "release.restart.value/v1",
      "schema": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": ["object", "null"],
        "additionalProperties": false
      }
    },
    "output": {
      "type_name": "release.restart.value/v1",
      "schema": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": ["object", "null"],
        "additionalProperties": false
      }
    },
    "nodes": {
      "approval": {
        "id": "approval",
        "name": "Approval",
        "kind": "approval",
        "input": {
          "type_name": "release.restart.value/v1",
          "schema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": ["object", "null"],
            "additionalProperties": false
          }
        },
        "output": {
          "type_name": "release.restart.value/v1",
          "schema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": ["object", "null"],
            "additionalProperties": false
          }
        },
        "configuration": {"gate_version": 1}
      }
    },
    "entries": ["approval"],
    "exits": ["approval"],
    "edges": []
  },
  "bindings": [],
  "requirements": {
    "capabilities": [],
    "plugins": [],
    "blocks": [],
    "agents": []
  },
  "run_limits": {
    "maximum_duration_ms": null,
    "node_execution_cap": 1000,
    "concurrency_cap": 8,
    "cycle_cap": 100,
    "retry_cap": 3
  },
  "producer": {
    "kind": "cli",
    "producer_id": "release-restart-proof"
  }
}
JSON

cat >"${workdir}/preset.json" <<'JSON'
{
  "workflow_id": "release/restart-proof",
  "preset_id": "release-preset",
  "revision": 1,
  "name": "Release preset",
  "configuration": {},
  "run_limits": null,
  "producer": {
    "kind": "cli",
    "producer_id": "release-restart-proof"
  }
}
JSON

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
            echo "release daemon exited before becoming ready" >&2
            cat "${log}" >&2 || true
            exit 1
        fi
        sleep 0.1
    done
    echo "release daemon socket was not created" >&2
    cat "${log}" >&2 || true
    exit 1
}

start_server "${workdir}/server-before.log"
first_pid="${server_pid}"

session_id="$(cd "${workdir}/repo" && "${bcode}" session create release-restart-proof)"
"${bcode}" workflow author create --draft-id initial-draft "${workdir}/workflow.json" >/dev/null
"${bcode}" workflow author publish \
    --workflow-id release/restart-proof \
    --draft-id initial-draft \
    --expected-generation 1 \
    --activate >/dev/null
"${bcode}" workflow author preset create "${workdir}/preset.json" >/dev/null
"${bcode}" workflow author fork \
    --workflow-id release/restart-proof \
    --draft-id durable-draft \
    --source-revision 1 >/dev/null
"${bcode}" workflow start \
    --parent-session-id "${session_id}" \
    --run-id release-run \
    active \
    --workflow-id release/restart-proof >"${workdir}/started.json"
"${bcode}" workflow author inspect \
    --workflow-id release/restart-proof >"${workdir}/before-inspect.json"
"${bcode}" runtime-work history "${session_id}" \
    --limit 50 >"${workdir}/before-runs.txt"

"${bcode}" workflow waits --run-id release-run --limit 10 >"${workdir}/before-waits.json"
python3 - "${workdir}" <<'PY'
import json
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
waits = json.loads((root / "before-waits.json").read_text())
node = json.loads((root / "workflow.json").read_text())["definition"]["nodes"]["approval"]
node["id"] = "added"
node["name"] = "Published agent"
node["kind"] = "agent"
node["output"] = {"type_name": "restart.result/v1", "schema": {"type": "object", "properties": {"recovered": {"type": "boolean"}}, "required": ["recovered"], "additionalProperties": False}}
node["configuration"] = {"version": 3, "execution_target": "fresh_isolated", "agent_profile": "build",
    "provider": "bcode.fake-provider", "model": "fake-echo", "read_only": True,
    "tool_capability": "read_only", "tool_allowlist": [], "timeout_ms": 30000,
    "prompt_mode": "json_input", "system_prompt": "",
    "output": {"mode": "structured", "result": {"schema": node["output"], "strict": True}}}
edit = {"version": 2, "run_id": "release-run", "mutation_id": "release-edit",
        "expected_revision": 1,
        "edits": [{"operation": "add_node", "node": node, "entry": False, "exit": True},
                  {"operation": "add_edge", "edge_id": 0, "edge": {"from": "approval", "to": "added", "kind": {"kind": "direct"}, "transform": None}}],
        "reconciliation": [{"disposition": "retain_with_bindings", "activation_id": waits[0]["activation_id"], "edge_ids": [0]}]}
(root / "edit.json").write_text(json.dumps(edit))
PY
"${bcode}" workflow stage-run-edit --file "${workdir}/edit.json" >"${workdir}/staged.json"
"${bcode}" workflow publish-run-edit --file "${workdir}/edit.json" >"${workdir}/published.json"
"${bcode}" workflow inspect-run-graph --run-id release-run --expected-revision 2 --limit 10 >"${workdir}/published-graph.json"

if [[ "${active_loss}" == "1" ]]; then
    activation_id="$(python3 - "${workdir}/before-waits.json" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1]))[0]["activation_id"])
PY
)"
    "${bcode}" workflow resolve-approval --run-id release-run --node-id approval \
        --activation-id "${activation_id}" --approve >"${workdir}/resolved.json"
    admitted=0
    for _ in {1..50}; do
        "${bcode}" workflow inspect-run --run-id release-run --limit 10 >"${workdir}/active.json"
        if python3 - "${workdir}/active.json" <<'PY'
import json
import sys
attempts = json.load(open(sys.argv[1]))["attempts"]
sys.exit(0 if len(attempts) == 1 and attempts[0]["status"] in ("admitted", "running") else 1)
PY
        then admitted=1; break; fi
        sleep 0.1
    done
    [[ "${admitted}" == "1" ]] || { echo "agent admission not observed" >&2; exit 1; }
    kill -KILL "${server_pid}"
    wait "${server_pid}" 2>/dev/null || true
else
    "${bcode}" server stop >/dev/null
    wait "${server_pid}"
fi
server_pid=""
if kill -0 "${first_pid}" 2>/dev/null; then
    echo "first release daemon process remained alive after stop" >&2
    exit 1
fi

# Do not explicitly start the replacement: this public workflow read must acquire it.
"${bcode}" workflow author inspect \
    --workflow-id release/restart-proof >"${workdir}/after-inspect.json"
second_pid="$(python3 - "${BCODE_STATE_DIR}/daemons" <<'PY'
import json
import pathlib
import sys
records = list(pathlib.Path(sys.argv[1]).glob("*.json"))
assert len(records) == 1, records
print(json.loads(records[0].read_text())["pid"])
PY
)"
server_pid="${second_pid}"
if [[ "${second_pid}" == "${first_pid}" ]]; then
    echo "release daemon restart reused the process id" >&2
    exit 1
fi
status_output="$("${bcode}" server status --verbose)"
client_digest="$(printf '%s\n' "${status_output}" | awk '/^client executable identity:/ {print $4}')"
daemon_digest="$(printf '%s\n' "${status_output}" | awk '/^executable identity:/ {print $3}')"
if [[ -z "${client_digest}" || "${client_digest}" != "${daemon_digest}" ]]; then
    echo "replacement daemon does not match client executable identity" >&2
    exit 1
fi
"${bcode}" workflow author draft get \
    --workflow-id release/restart-proof \
    --draft-id durable-draft >"${workdir}/after-draft.json"
"${bcode}" workflow author revision get \
    --workflow-id release/restart-proof \
    --revision 1 >"${workdir}/after-revision.json"
"${bcode}" workflow author preset get \
    --workflow-id release/restart-proof \
    --preset-id release-preset >"${workdir}/after-preset.json"
"${bcode}" runtime-work history "${session_id}" \
    --limit 50 >"${workdir}/after-runs.txt"

python3 - "${workdir}" "${session_id}" "${first_pid}" "${second_pid}" <<'PY'
import json
import pathlib
import sys

workdir = pathlib.Path(sys.argv[1])
session_id = sys.argv[2]
first_pid = int(sys.argv[3])
second_pid = int(sys.argv[4])

before = json.loads((workdir / "before-inspect.json").read_text())
after = json.loads((workdir / "after-inspect.json").read_text())
started = json.loads((workdir / "started.json").read_text())
draft = json.loads((workdir / "after-draft.json").read_text())
revision = json.loads((workdir / "after-revision.json").read_text())
preset = json.loads((workdir / "after-preset.json").read_text())
before_runs = (workdir / "before-runs.txt").read_text()
after_runs = (workdir / "after-runs.txt").read_text()

assert first_pid != second_pid
assert before == after
assert after["workflow"]["workflow_id"] == "release/restart-proof"
assert after["workflow"]["active_revision"] == 1
assert after["issues"] == []
assert draft["identity"] == {
    "workflow_id": "release/restart-proof",
    "draft_id": "durable-draft",
}
assert draft["base_revision"] == 1
assert draft["generation"] == 1
assert revision["identity"] == {
    "workflow_id": "release/restart-proof",
    "revision": 1,
}
definition = revision["definition_identity"]
assert definition["kind"] == "release/restart-proof"
assert definition["definition_version"] == 2
assert preset["workflow_id"] == "release/restart-proof"
assert preset["preset_id"] == "release-preset"
assert preset["revision"] == 1
assert preset["generation"] == 1
run = started["started"]["run"]
assert run["run_id"] == "release-run"
assert run["parent_session_id"] == session_id
assert run["definition_id"] == definition["definition_id"]
assert run["definition_version"] == definition["definition_version"]
assert run["authored_provenance"]["workflow_id"] == "release/restart-proof"
assert run["authored_provenance"]["revision"] == 1
for history in (before_runs, after_runs):
    compact = history.replace("\n", "")
    assert "workflow:release-run" in compact
    assert definition["definition_id"] in compact
    assert f" v{definition['definition_version']}" in compact
PY

if [[ "${active_loss}" != "1" ]]; then
"${bcode}" workflow waits --run-id release-run --limit 10 >"${workdir}/after-waits.json"
activation_id="$(python3 - "${workdir}" <<'PY'
import json
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
before = json.loads((root / "before-waits.json").read_text())
after = json.loads((root / "after-waits.json").read_text())
assert len(before) == len(after) == 1
original = next(wait for wait in after if wait["node_id"] == "approval")
assert before[0]["activation_id"] == original["activation_id"]
print(original["activation_id"])
PY
)"
"${bcode}" workflow resolve-approval --run-id release-run --node-id approval \
    --activation-id "${activation_id}" --approve >"${workdir}/resolved.json"
fi
"${bcode}" workflow inspect-run-graph --run-id release-run --expected-revision 2 --limit 10 >"${workdir}/recovered-graph.json"
for _ in {1..100}; do
    "${bcode}" workflow inspect-run --run-id release-run --limit 10 >"${workdir}/completed.json"
    if python3 - "${workdir}/completed.json" <<'PY'
import json
import sys
sys.exit(0 if json.load(open(sys.argv[1]))["run"]["status"] == "completed" else 1)
PY
    then break; fi
    sleep 0.1
done
"${bcode}" workflow run-output --run-id release-run --limit 10 >"${workdir}/outputs.json"
python3 - "${workdir}/outputs.json" "${activation_id}" <<'PY'
import json
import sys
outputs = json.load(open(sys.argv[1]))
assert len(outputs) == 2, outputs
assert {output["node_id"] for output in outputs} == {"approval", "added"}, outputs
assert any(output["activation_id"] == sys.argv[2] for output in outputs), outputs
assert next(output["value"] for output in outputs if output["node_id"] == "added") == {"recovered": True}, outputs
PY

"${bcode}" workflow inspect-run --run-id release-run --limit 10 >"${workdir}/completed.json"
python3 - "${workdir}/completed.json" <<'PY'
import json
import sys
inspection = json.load(open(sys.argv[1]))
assert inspection["run"]["status"] == "completed", inspection["run"]
assert inspection["waits"] == [], inspection["waits"]
assert len(inspection["activations"]) == 2, inspection["activations"]
assert len(inspection["outputs"]) == 2, inspection["outputs"]
assert len(inspection["attempts"]) == 1, inspection["attempts"]
assert inspection["attempts"][0]["status"] == "succeeded", inspection["attempts"]
PY

"${bcode}" server stop >/dev/null
# The automatically acquired daemon is detached, not a child of this shell.
server_pid=""
echo "release-workflow-daemon-restart: PASS"
