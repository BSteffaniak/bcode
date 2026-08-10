#!/usr/bin/env bash
set -euo pipefail

# Release-binary proof for an externally hosted non-Git package closure. The proof uses only
# public package/run commands, restarts the daemon across a durable typed approval wait, reaches
# canonical terminal output, and separately cancels a composed run after its child is terminal.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-non-git-package.XXXXXX)"
repo="${workdir}/external-project"
workflow_root="${repo}/.bcode/workflows"
mkdir -p \
    "${workdir}/config" \
    "${workdir}/state" \
    "${workdir}/tmp" \
    "${workflow_root}/command" \
    "${workflow_root}/data-quality" \
    "${workflow_root}/remediation"

cp "${root}/examples/workflows/packages/command/package.workflow-package.yaml" \
    "${workflow_root}/command/package.workflow-package.yaml"
cp "${root}/examples/workflows/packages/command/run-and-assert.workflow.yaml" \
    "${workflow_root}/command/run-and-assert.workflow.yaml"
cp "${root}/examples/workflows/packages/remediation.workflow-package.yaml" \
    "${workflow_root}/remediation.workflow-package.yaml"
cp "${root}/examples/workflows/packages/remediation/bounded-remediation.workflow.yaml" \
    "${workflow_root}/remediation/bounded-remediation.workflow.yaml"
cp "${root}/examples/workflows/packages/data-quality.workflow-package.yaml" \
    "${workflow_root}/data-quality.workflow-package.yaml"
cp "${root}/examples/workflows/packages/data-quality/data-quality.workflow.yaml" \
    "${workflow_root}/data-quality/data-quality.workflow.yaml"

export TMPDIR="${workdir}/tmp"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_CONFIG_TOML=$'[model]\nprofile = "non-git-proof"\n\n[model.profiles.non-git-proof]\nprovider_plugin_id = "bcode.fake-provider"\nmodel_id = "fake-echo"\n\n[model.prompt_cache]\nmode = "off"'
cat >"${BCODE_CONFIG}" <<'EOF'
[model]
profile = "non-git-proof"

[model.profiles.non-git-proof]
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
    if [[ "${BCODE_KEEP_NON_GIT_PROOF:-0}" == "1" ]]; then
        echo "non-Git package proof artifacts: ${workdir}" >&2
    else
        rm -rf "${workdir}"
    fi
}
trap cleanup EXIT

cd "${root}"
if [[ "${BCODE_SKIP_RELEASE_BUILD:-0}" != "1" ]]; then
    cargo build --quiet --release -p bcode --features distribution
fi
bcode="${BCODE_NON_GIT_BINARY:-${root}/target/release/bcode}"
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
    echo "non-Git package daemon did not become ready" >&2
    exit 1
}

stop_server() {
    "${bcode}" server stop >/dev/null
    wait "${server_pid}"
    server_pid=""
}

json_field() {
    python3 - "$1" "$2" <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text())
for segment in sys.argv[2].split('.'):
    value = value[int(segment)] if segment.isdigit() else value[segment]
print(value)
PY
}

wait_for_child_mutation_approval() {
    local root_run_id="$1" root_output="$2" child_output="$3"
    for _ in {1..300}; do
        "${bcode}" workflow inspect-run --run-id "${root_run_id}" >"${root_output}"
        local child_run_id
        child_run_id="$(python3 - "${root_output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text())
runs=value.get("descendant_runs", [])
print(runs[0]["run"]["run_id"] if runs else "")
PY
)"
        if [[ -n "${child_run_id}" ]]; then
            "${bcode}" workflow inspect-run --run-id "${child_run_id}" >"${child_output}"
            local approval_id
            approval_id="$(python3 - "${child_output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text())
approvals=value.get("mutation_approvals", [])
print(approvals[0]["approval_id"] if approvals else "")
PY
)"
            if [[ -n "${approval_id}" ]]; then
                printf '%s\n%s\n' "${child_run_id}" "${approval_id}"
                return
            fi
        fi
        sleep 0.1
    done
    echo "non-Git child mutation approval did not become ready" >&2
    exit 1
}

wait_for_operator_approval() {
    local run_id="$1" output="$2"
    for _ in {1..300}; do
        "${bcode}" workflow inspect-run --run-id "${run_id}" >"${output}"
        if python3 - "${output}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text())
raise SystemExit(0 if any(wait["node_id"] == "operator_decision" for wait in value.get("waits", [])) else 1)
PY
        then
            return
        fi
        sleep 0.1
    done
    echo "non-Git operator approval did not become ready" >&2
    exit 1
}

wait_for_status() {
    local run_id="$1" expected="$2" output="$3"
    for _ in {1..300}; do
        "${bcode}" workflow inspect-run --run-id "${run_id}" >"${output}"
        if [[ "$(json_field "${output}" run.status)" == "${expected}" ]]; then
            return
        fi
        sleep 0.1
    done
    echo "workflow ${run_id} did not reach ${expected}" >&2
    exit 1
}

start_server "${workdir}/server-before.log"
session_id="$(cd "${repo}" && "${bcode}" session create non-git-release-proof)"

"${bcode}" workflow package discover --workspace "${repo}" >"${workdir}/discovered.json"
"${bcode}" workflow package validate "${workflow_root}/data-quality.workflow-package.yaml" \
    >"${workdir}/validated.json"
"${bcode}" workflow package preview "${workflow_root}/data-quality.workflow-package.yaml" \
    >"${workdir}/previewed.json"
"${bcode}" workflow package apply "${workflow_root}/data-quality.workflow-package.yaml" \
    >"${workdir}/applied.json"
python3 - "${workdir}/applied.json" "${workdir}" <<'PY'
import json, pathlib, sys
values=json.loads(pathlib.Path(sys.argv[1]).read_text())
root=pathlib.Path(sys.argv[2])
for value in values:
    name=value["package_id"].rsplit("-", 1)[-1]
    if value["package_id"].endswith("examples-command"):
        name="command"
    elif value["package_id"].endswith("examples-remediation"):
        name="remediation"
    elif value["package_id"].endswith("examples-data-quality"):
        name="data-quality"
    (root/f"{name}-lock.json").write_text(json.dumps(value["lock"]))
PY
"${bcode}" workflow package publish --lock "${workdir}/command-lock.json" \
    --expected-generation run-and-assert=1 >"${workdir}/command-published.json"
"${bcode}" workflow package publish --lock "${workdir}/remediation-lock.json" \
    --expected-generation bounded-remediation=1 >"${workdir}/remediation-published.json"
"${bcode}" workflow package publish --lock "${workdir}/data-quality-lock.json" \
    --expected-generation data-quality=1 >"${workdir}/data-quality-published.json"

cat >"${workdir}/input.json" <<'JSON'
{
  "version": 2,
  "cwd": ".",
  "commands": [{
    "argv": ["printf", "quality-ok"],
    "timeout_ms": 60000,
    "accepted_exit_codes": [0],
    "continue_on_unaccepted_exit": false
  }],
  "environment": {"inherit": true, "set": {}},
  "output": {"preview_bytes": 8192, "artifact_spill": true}
}
JSON

"${bcode}" workflow start --parent-session-id "${session_id}" --run-id non-git-terminal \
    --input "${workdir}/input.json" package-export \
    --package-id bcode/examples-data-quality --export data-quality \
    >"${workdir}/terminal-started.json"
mapfile -t terminal_child < <(wait_for_child_mutation_approval \
    non-git-terminal "${workdir}/terminal-root-before.json" "${workdir}/terminal-child-before.json")
"${bcode}" workflow-ui approve-mutation --id "${terminal_child[0]}" \
    --approval "${terminal_child[1]}" >"${workdir}/terminal-mutation-approved.txt"
wait_for_operator_approval non-git-terminal "${workdir}/terminal-wait-before-restart.json"
terminal_activation="$(python3 - "${workdir}/terminal-wait-before-restart.json" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text())
print(next(wait["activation_id"] for wait in value["waits"] if wait["node_id"] == "operator_decision"))
PY
)"

stop_server
start_server "${workdir}/server-after.log"
"${bcode}" workflow inspect-run --run-id non-git-terminal >"${workdir}/terminal-wait-after-restart.json"
python3 - "${workdir}/terminal-wait-after-restart.json" "${terminal_activation}" <<'PY'
import json, pathlib, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text())
wait=next(wait for wait in value["waits"] if wait["node_id"] == "operator_decision")
assert wait["activation_id"] == sys.argv[2]
PY
"${bcode}" workflow resolve-approval --run-id non-git-terminal --node-id operator_decision \
    --activation-id "${terminal_activation}" --approve >"${workdir}/terminal-approval.json"
wait_for_status non-git-terminal completed "${workdir}/terminal.json"

"${bcode}" workflow start --parent-session-id "${session_id}" --run-id non-git-cancelled \
    --input "${workdir}/input.json" package-export \
    --package-id bcode/examples-data-quality --export data-quality \
    >"${workdir}/cancel-started.json"
mapfile -t cancel_child < <(wait_for_child_mutation_approval \
    non-git-cancelled "${workdir}/cancel-root-before.json" "${workdir}/cancel-child-before.json")
"${bcode}" workflow-ui approve-mutation --id "${cancel_child[0]}" \
    --approval "${cancel_child[1]}" >"${workdir}/cancel-mutation-approved.txt"
wait_for_operator_approval non-git-cancelled "${workdir}/cancel-wait.json"
"${bcode}" workflow-ui cancel --id non-git-cancelled >"${workdir}/cancelled.txt"
wait_for_status non-git-cancelled cancelled "${workdir}/cancel-terminal.json"

python3 - "${workdir}" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
discovered=json.loads((root/"discovered.json").read_text())
assert any(value["package_id"] == "bcode/examples-data-quality" for value in discovered)
assert len(json.loads((root/"validated.json").read_text())["plan"]["packages"]) == 3
preview=json.loads((root/"previewed.json").read_text())
assert preview["members"][0]["compilation"]["validation"]["valid"]
for name in ("command", "remediation", "data-quality"):
    assert json.loads((root/f"{name}-published.json").read_text())["outcome"] == "published"
terminal=json.loads((root/"terminal.json").read_text())
assert terminal["run"]["status"] == "completed"
assert terminal["terminal_output"]["node_id"] == "operator_decision"
assert terminal["terminal_output"]["value"]["status"] == "ready"
assert terminal["terminal_output"]["value"]["iteration"] == 0
assert len(terminal["descendant_runs"]) == 1
assert terminal["descendant_runs"][0]["run"]["status"] == "completed"
cancelled=json.loads((root/"cancel-terminal.json").read_text())
assert cancelled["run"]["status"] == "cancelled"
assert cancelled["run"]["cancellation_requested_at_ms"] is not None
assert cancelled["descendant_runs"][0]["run"]["status"] == "completed"
PY

stop_server
echo "release-non-git-workflow-package: PASS"
