#!/usr/bin/env bash
set -euo pipefail

# This proof uses the release binary and isolated daemon state. It verifies all three shipped
# documents through public template discovery, runs the published three-level parent with a
# deterministic fake-provider profile that echoes each agent's canonical structured input, and
# proves that exact transitive child admission plus bounded public inspection survive restart.
# A deterministic public input-wait fixture separately proves restart-safe wait recovery and
# terminal resolution without bypassing authorization. The proof also drives one standalone
# implementation batch to its runtime-owned exhaustion bound.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-flagship-lifecycle.XXXXXX)"
mkdir -p "${workdir}/config" "${workdir}/repo" "${workdir}/state" "${workdir}/tmp"
git -C "${workdir}/repo" init --quiet
git -C "${workdir}/repo" -c core.hooksPath=/dev/null -c user.name='Bcode Release Proof' \
    -c user.email='release-proof@invalid' commit --quiet --allow-empty \
    -m 'Initialize release proof repository'
export TMPDIR="${workdir}/tmp"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_CONFIG_TOML=$'[model]\nprofile = "flagship-fixture"\n\n[model.profiles.flagship-fixture]\nprovider_plugin_id = "bcode.fake-provider"\nmodel_id = "fake-echo"\n\n[model.profiles.flagship-fixture.settings]\nfake_structured_output_json = "matching_input:"'
cat >"${BCODE_CONFIG}" <<'EOF'
[model]
profile = "flagship-fixture"

[model.profiles.flagship-fixture]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[model.profiles.flagship-fixture.settings]
fake_structured_output_json = "matching_input:"

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
state["instruction_fingerprint_sha256"] = "0" * 64
state["validation_plan"] = {
    "version": 1,
    "cwd": ".",
    "commands": [{
        "argv": ["/usr/bin/true"],
        "timeout_ms": 10000,
        "continue_on_nonzero": False,
    }],
    "environment": {"inherit": True, "set": {}},
    "output": {"preview_bytes": 4096, "artifact_spill": True},
}
state["formatting_plan"] = state["validation_plan"]
state["latest"]["completion_assessment"] = {
    "condition_met": True,
    "title": "Flagship release proof",
    "description": "Deterministic release lifecycle checkpoint.",
}
state["phase"] = "implementing"
pathlib.Path(sys.argv[2]).write_text(json.dumps(configuration, separators=(",", ":")))
PY

configuration="$(cat "${workdir}/configuration.json")"

python3 - "${workdir}/configuration.json" "${workdir}/exhausted-configuration.json" <<'PY'
import json
import pathlib
import sys

configuration = json.loads(pathlib.Path(sys.argv[1]).read_text())
state = configuration["state"]
state["latest"]["completion_assessment"]["condition_met"] = False
state["latest"]["completion_assessment"]["description"] = (
    "Deterministic release lifecycle exhaustion fixture."
)
pathlib.Path(sys.argv[2]).write_text(json.dumps(state, separators=(",", ":")))
PY
exhausted_configuration="$(cat "${workdir}/exhausted-configuration.json")"

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

wait_for_flagship_approval() {
    local output="$1"
    local descendant_output="${workdir}/approval-before.txt"
    for _ in {1..300}; do
        if ! "${bcode}" workflow inspect --id flagship-root --session "${session_id}" >"${output}"; then
            sleep 0.1
            continue
        fi
        if [[ ! -s "${output}" ]]; then
            sleep 0.1
            continue
        fi
        while IFS= read -r descendant_run_id; do
            [[ -n "${descendant_run_id}" ]] || continue
            if "${bcode}" workflow inspect --id "${descendant_run_id}" --session "${session_id}" >"${descendant_output}" \
                && python3 - "${descendant_output}" <<'PY'
import json
import pathlib
import sys

value = json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n", 1)[1])
approvals = value[0].get("options", {}).get("mutation_approvals", [])
raise SystemExit(0 if approvals else 1)
PY
            then
                return
            fi
        done < <(python3 - "${output}" <<'PY'
import json
import pathlib
import sys

value = json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n", 1)[1])
for descendant in value[0].get("options", {}).get("descendant_runs", []):
    print(descendant["run"]["run_id"])
PY
)
        sleep 0.1
    done
    echo "flagship workflow did not reach mutation approval" >&2
    cat "${output}" >&2
    exit 1
}

resolve_flagship_approvals_until_settled() {
    local root_output="${workdir}/run-settled.txt"
    local descendant_output="${workdir}/approval-current.txt"
    for _ in {1..600}; do
        "${bcode}" workflow inspect --id flagship-root --session "${session_id}" >"${root_output}"
        local root_status
        root_status="$(python3 - "${root_output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
print(value[0]["options"]["run"]["status"])
PY
)"
        local resolved=0
        while IFS= read -r descendant_run_id; do
            [[ -n "${descendant_run_id}" ]] || continue
            if ! "${bcode}" workflow inspect --id "${descendant_run_id}" --session "${session_id}" >"${descendant_output}" \
                || [[ ! -s "${descendant_output}" ]]; then
                continue
            fi
            local approval_details
            approval_details="$(python3 - "${descendant_output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
for approval in value[0]["options"].get("mutation_approvals", []):
    print(approval["run_id"], approval["approval_id"])
    break
PY
)"
            if [[ -n "${approval_details}" ]]; then
                local approval_run_id approval_id
                read -r approval_run_id approval_id <<<"${approval_details}"
                "${bcode}" workflow approve-mutation --id "${approval_run_id}" --approval "${approval_id}" --session "${session_id}" >>"${workdir}/subsequent-approvals.jsonl"
                resolved=1
                break
            fi
        done < <(python3 - "${root_output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
for descendant in value[0]["options"].get("descendant_runs", []):
    print(descendant["run"]["run_id"])
PY
)
        if [[ "${root_status}" != "running" ]]; then
            return
        fi
        if [[ "${resolved}" == "0" ]]; then
            sleep 0.1
        fi
    done
    echo "flagship workflow did not settle after resolving mutation approvals" >&2
    cat "${root_output}" >&2
    exit 1
}

# Start the exact published parent with a deterministic provider fixture that can only select a
# schema-valid value already present in the workflow's canonical agent input. It does not invent
# workflow-product fields or move product behavior into the provider.
(
    cd "${workdir}/repo"
    "${bcode}" workflow template-start --owner bcode.workflow --template progress-driven-delivery --version 1 --session "${session_id}" --run flagship-root --input "${configuration}" >"${workdir}/started.json"
)
wait_for_flagship_approval "${workdir}/run-before.txt"

kill "${server_pid}"
wait "${server_pid}" || true
server_pid=""
start_server "${workdir}/after.log"
second_pid="${server_pid}"

"${bcode}" workflow template-describe --owner bcode.workflow --template implementation-batch --version 1 --session "${session_id}" >"${workdir}/batch-after.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template delivery-tranche --version 1 --session "${session_id}" >"${workdir}/tranche-after.json"
"${bcode}" workflow template-describe --owner bcode.workflow --template progress-driven-delivery --version 1 --session "${session_id}" >"${workdir}/parent-after.json"
"${bcode}" workflow inspect --id flagship-root --session "${session_id}" >"${workdir}/run-after.txt"
approval_details="$(python3 - "${workdir}/approval-before.txt" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
approval=value[0]["options"]["mutation_approvals"][0]
print(approval["run_id"], approval["approval_id"])
PY
)"
read -r approval_run_id approval_id <<<"${approval_details}"
"${bcode}" workflow approve-mutation --id "${approval_run_id}" --approval "${approval_id}" --session "${session_id}" >"${workdir}/approval-resolved.json"
"${bcode}" workflow inspect --id "${approval_run_id}" --session "${session_id}" >"${workdir}/approval-approved.txt"
resolve_flagship_approvals_until_settled
"${bcode}" workflow inspect --id flagship-root --session "${session_id}" >"${workdir}/run-approved.txt"
"${bcode}" workflow inspect --id "${wait_run_id}" --session "${session_id}" >"${workdir}/wait-after.txt"
"${bcode}" workflow provide-input --id "${wait_run_id}" --node operator_wait --activation "${wait_activation}" --input '{}' --session "${session_id}" >"${workdir}/wait-resolved.json"
"${bcode}" workflow inspect --id "${wait_run_id}" --session "${session_id}" >"${workdir}/wait-completed.txt"

# Start the standalone exact implementation batch with semantic completion held false. The
# runtime, not the provider fixture, owns and emits the 20-iteration exhausted result.
(
    cd "${workdir}/repo"
    "${bcode}" workflow template-start --owner bcode.workflow --template implementation-batch --version 1 --session "${session_id}" --run flagship-exhausted-batch --input "${exhausted_configuration}" >"${workdir}/exhausted-started.json"
)
resolve_run_approvals_until_settled() {
    local run_id="$1"
    local output="$2"
    for _ in {1..2400}; do
        "${bcode}" workflow inspect --id "${run_id}" --session "${session_id}" >"${output}"
        local status approval_details
        status="$(python3 - "${output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
print(value[0]["options"]["run"]["status"])
PY
)"
        if [[ "${status}" != "running" ]]; then
            return
        fi
        approval_details="$(python3 - "${output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text().split("\n",1)[1])
for approval in value[0]["options"].get("mutation_approvals", []):
    print(approval["run_id"], approval["approval_id"])
    break
PY
)"
        if [[ -n "${approval_details}" ]]; then
            local approval_run_id approval_id
            read -r approval_run_id approval_id <<<"${approval_details}"
            "${bcode}" workflow approve-mutation --id "${approval_run_id}" --approval "${approval_id}" --session "${session_id}" >>"${workdir}/exhausted-approvals.jsonl"
        else
            sleep 0.1
        fi
    done
    echo "exhausted implementation batch did not settle" >&2
    cat "${output}" >&2
    exit 1
}
resolve_run_approvals_until_settled flagship-exhausted-batch "${workdir}/exhausted-completed.txt"

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
started_options = started[0]["options"]
assert started_options["runs"][0]["definition_id"].startswith(
    "bcode.workflow/progress-driven-delivery@1@"
)
before_run = root.joinpath("run-before.txt").read_text()
after_run = root.joinpath("run-after.txt").read_text()
approved_run = root.joinpath("run-approved.txt").read_text()
exhausted_run = root.joinpath("exhausted-completed.txt").read_text()
approval_before = root.joinpath("approval-before.txt").read_text()
approval_approved = root.joinpath("approval-approved.txt").read_text()
for inspection in (before_run, after_run, approved_run):
    assert "flagship-root" in inspection
    assert "progress-driven-delivery" in inspection

before_options = json.loads(before_run.split("\n", 1)[1])[0]["options"]
after_options = json.loads(after_run.split("\n", 1)[1])[0]["options"]
approved_options = json.loads(approved_run.split("\n", 1)[1])[0]["options"]
assert before_options["child_run_links"] == after_options["child_run_links"]
assert before_options["descendant_runs"] == after_options["descendant_runs"]
assert len(after_options["descendant_runs"]) == 2
assert {item["link"]["depth"] for item in after_options["descendant_runs"]} == {2, 3}
assert all(item["run"]["parent_session_id"] == after_options["run"]["parent_session_id"] for item in after_options["descendant_runs"])
assert all(item["run"]["authorization_ceiling"] == "mutating" for item in after_options["descendant_runs"])
assert approved_options["run"]["status"] == "completed"
assert all(item["run"]["status"] == "completed" for item in approved_options["descendant_runs"])
assert {(item["node_id"], item["iterations_completed"], item["outcome"]) for item in approved_options["repeat_outcomes"]} == {
    ("batch_repeat", 1, "condition_cleared"),
    ("tranche_repeat", 1, "condition_cleared"),
}
assert '"status": "pending"' in approval_before
approval_resolution = json.loads(root.joinpath("approval-resolved.json").read_text().split("\n", 1)[1])
assert "approved" in json.dumps(approval_resolution)
assert '"node_id": "validation"' in approval_approved
assert '"status": "completed"' in approval_approved
approval_options = json.loads(approval_approved.split("\n", 1)[1])[0]["options"]
assert any(grant["node_id"] == "validation" for grant in approval_options["grants"])
assert any(approval["node_id"] in {"format", "post_format_validation"} for approval in approval_options["mutation_approvals"])
exhausted_options = json.loads(exhausted_run.split("\n", 1)[1])[0]["options"]
assert exhausted_options["run"]["status"] == "completed"
assert any(
    outcome["node_id"] == "batch_repeat"
    and outcome["iterations_completed"] == 20
    and outcome["outcome"] == "iteration_limit_reached"
    for outcome in exhausted_options["repeat_outcomes"]
)
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
