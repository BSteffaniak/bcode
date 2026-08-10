#!/usr/bin/env bash
set -euo pipefail

# Release-binary lifecycle proof for the product-facing recursively imported delivery package.
# The proof uses an isolated repository, local bare remote, daemon state, package locks, and public
# package/run commands. Mutating prompts and shell calls cross the ordinary approval boundary.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-delivery-package.XXXXXX)"
repo="${workdir}/repo"
remote="${workdir}/remote.git"
mkdir -p "${workdir}/config" "${workdir}/state" "${workdir}/tmp" "${repo}"
git init --bare --quiet "${remote}"
git -C "${repo}" init --quiet
printf '%s\n' initial >"${repo}/tracked.txt"
git -C "${repo}" -c user.name='Bcode Release Proof' -c user.email='proof@invalid' add tracked.txt
git -C "${repo}" -c core.hooksPath=/dev/null -c user.name='Bcode Release Proof' -c user.email='proof@invalid' commit --quiet -m initial
git -C "${repo}" remote add origin "${remote}"
git -C "${repo}" push --quiet -u origin HEAD:main

export TMPDIR="${workdir}/tmp"
export BCODE_SOCKET="${workdir}/bcode.sock"
export BCODE_STATE_DIR="${workdir}/state"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_CONFIG_TOML=$'[model]\nprofile = "delivery-proof"\n\n[model.profiles.delivery-proof]\nprovider_plugin_id = "bcode.fake-provider"\nmodel_id = "fake-echo"\n\n[model.prompt_cache]\nmode = "off"'
cat >"${BCODE_CONFIG}" <<'EOF'
[model]
profile = "delivery-proof"
[model.profiles.delivery-proof]
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
  if [[ "${BCODE_KEEP_DELIVERY_PROOF:-0}" == "1" ]]; then
    echo "delivery package proof artifacts: ${workdir}" >&2
  else
    rm -rf "${workdir}"
  fi
}
trap cleanup EXIT
cd "${root}"
if [[ "${BCODE_SKIP_RELEASE_BUILD:-0}" != "1" ]]; then
  cargo build --quiet --release -p bcode --features distribution
fi
bcode="${BCODE_DELIVERY_BINARY:-${root}/target/release/bcode}"
[[ -x "${bcode}" ]] || { echo "release binary unavailable: ${bcode}" >&2; exit 1; }
start_server() {
  rm -f "${BCODE_SOCKET}"
  "${bcode}" server run >"$1" 2>&1 & server_pid="$!"
  for _ in {1..300}; do
    [[ -S "${BCODE_SOCKET}" ]] && return
    kill -0 "${server_pid}" 2>/dev/null || { cat "$1" >&2; exit 1; }
    sleep 0.1
  done
  echo "delivery daemon unavailable" >&2; exit 1
}
stop_server() { "${bcode}" server stop >/dev/null; wait "${server_pid}"; server_pid=""; }
json_value() {
  python3 - "$1" "$2" <<'PY'
import json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text())
for s in sys.argv[2].split('.'):
    v=v[int(s)] if s.isdigit() else v[s]
print(v)
PY
}
start_server "${workdir}/server-before.log"
session_id="$(cd "${repo}" && "${bcode}" session create delivery-package-proof)"
"${bcode}" workflow package validate examples/workflows/packages/delivery.workflow-package.yaml >"${workdir}/validate.json"
"${bcode}" workflow package preview examples/workflows/packages/delivery.workflow-package.yaml >"${workdir}/preview.json"
"${bcode}" workflow package apply examples/workflows/packages/delivery.workflow-package.yaml >"${workdir}/apply.json"
python3 - "${workdir}/apply.json" "${workdir}" <<'PY'
import json,pathlib,sys
for index,value in enumerate(json.loads(pathlib.Path(sys.argv[1]).read_text())):
    pathlib.Path(sys.argv[2],f"lock-{index}.json").write_text(json.dumps(value["lock"]))
    pathlib.Path(sys.argv[2],f"members-{index}.txt").write_text("".join(f"{member['member_id']}\n" for member in value["members"]))
PY
count="$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "${workdir}/apply.json")"
for ((index=0; index<count; index++)); do
  args=()
  while IFS= read -r member; do args+=(--expected-generation "${member}=1"); done <"${workdir}/members-${index}.txt"
  "${bcode}" workflow package publish --lock "${workdir}/lock-${index}.json" \
    "${args[@]}" >"${workdir}/publish-${index}.json"
done
cat >"${workdir}/input.json" <<JSON
{
  "planning":{"path":"progress.md","prompt":"Return a bounded release-proof planning result."},
  "validation_plan":{"version":2,"cwd":".","commands":[{"argv":["true"],"timeout_ms":60000,"accepted_exit_codes":[0],"continue_on_unaccepted_exit":false}],"environment":{"inherit":true,"set":{}},"output":{"preview_bytes":4096,"artifact_spill":true}},
  "checkpoint_plan":{"version":2,"cwd":".","commands":[{"argv":["git","status","--short"],"timeout_ms":60000,"accepted_exit_codes":[0],"continue_on_unaccepted_exit":false}],"environment":{"inherit":true,"set":{}},"output":{"preview_bytes":4096,"artifact_spill":true}},
  "synchronization_plan":{"initial":{"version":2,"cwd":".","commands":[{"argv":["git","status","--short"],"timeout_ms":60000,"accepted_exit_codes":[0],"continue_on_unaccepted_exit":false}],"environment":{"inherit":true,"set":{"GIT_TERMINAL_PROMPT":"0"}},"output":{"preview_bytes":4096,"artifact_spill":true}},"after_recovery":{"version":2,"cwd":".","commands":[{"argv":["git","status","--short"],"timeout_ms":60000,"accepted_exit_codes":[0],"continue_on_unaccepted_exit":false}],"environment":{"inherit":true,"set":{"GIT_TERMINAL_PROMPT":"0"}},"output":{"preview_bytes":4096,"artifact_spill":true}}},
  "review_scope":"Review the release-proof delivery state.",
  "stop_condition":"Complete when the ordered delivery path reaches operator approval."
}
JSON
"${bcode}" workflow start --parent-session-id "${session_id}" --run-id delivery-package-run \
  --input "${workdir}/input.json" --parent-session-generation 0 package-export --package-id bcode/examples-delivery \
  --export feature-delivery >"${workdir}/start.json"
"${bcode}" workflow inspect-run --run-id delivery-package-run >"${workdir}/input-wait.json"
input_activation="$(python3 - "${workdir}/input-wait.json" <<'PY'
import json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text())
print(next(w["activation_id"] for w in v["waits"] if w["node_id"]=="request"))
PY
)"
"${bcode}" workflow provide-input --run-id delivery-package-run --node-id request \
  --activation-id "${input_activation}" --value "${workdir}/input.json" >"${workdir}/input-resolved.json"

# Resolve every durable mutation approval across the bounded descendant hierarchy.
for _ in {1..600}; do
  "${bcode}" workflow inspect-run --run-id delivery-package-run >"${workdir}/inspect.json"
  status="$(json_value "${workdir}/inspect.json" run.status)"
  [[ "${status}" == completed ]] && break
  python3 - "${workdir}/inspect.json" >"${workdir}/run-ids.txt" <<'PY'
import json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text())
print(v["run"]["run_id"])
for d in v.get("descendant_runs",[]): print(d["run"]["run_id"])
PY
  while IFS= read -r run_id; do
    "${bcode}" workflow inspect-run --run-id "${run_id}" >"${workdir}/candidate.json" || continue
    wait_index=0
    while IFS= read -r encoded_wait; do
      [[ -z "${encoded_wait}" ]] && continue
      wait_index=$((wait_index + 1))
      python3 - "${encoded_wait}" "${workdir}/wait-value.json" >"${workdir}/wait-meta.txt" <<'PY'
import base64,json,pathlib,sys
value=json.loads(base64.b64decode(sys.argv[1]))
pathlib.Path(sys.argv[2]).write_text(json.dumps(value[2]))
print(value[0])
print(value[1])
PY
      wait_node="$(sed -n '1p' "${workdir}/wait-meta.txt")"
      wait_activation="$(sed -n '2p' "${workdir}/wait-meta.txt")"
      "${bcode}" workflow provide-input --run-id "${run_id}" --node-id "${wait_node}" \
        --activation-id "${wait_activation}" --value "${workdir}/wait-value.json" >/dev/null
    done < <(python3 - "${workdir}/candidate.json" <<'PY'
import base64,json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text())
for w in v.get("waits",[]):
    if w.get("kind") == "input":
        print(base64.b64encode(json.dumps([w["node_id"],w["activation_id"],w.get("input")]).encode()).decode())
PY
)
    "${bcode}" workflow inspect-run --run-id "${run_id}" >"${workdir}/candidate.json" || continue
    python3 - "${workdir}/candidate.json" >"${workdir}/approvals.txt" <<'PY'
import json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text())
for a in v.get("mutation_approvals",[]): print(a["approval_id"])
PY
    while IFS= read -r approval; do
      [[ -z "${approval}" ]] || "${bcode}" workflow-ui approve-mutation --id "${run_id}" --approval "${approval}" >/dev/null
    done <"${workdir}/approvals.txt"
  done <"${workdir}/run-ids.txt"
  "${bcode}" workflow inspect-run --run-id delivery-package-run >"${workdir}/inspect.json"
  python3 - "${workdir}/inspect.json" >"${workdir}/wait.txt" <<'PY'
import json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text())
for w in v.get("waits",[]):
    if w["node_id"]=="operator_decision": print(w["activation_id"])
PY
  activation="$(cat "${workdir}/wait.txt")"
  if [[ -n "${activation}" ]]; then
    stop_server
    start_server "${workdir}/server-after.log"
    "${bcode}" workflow resolve-approval --run-id delivery-package-run --node-id operator_decision \
      --activation-id "${activation}" --approve >"${workdir}/operator-approved.json"
  fi
  sleep 0.1
done
"${bcode}" workflow inspect-run --run-id delivery-package-run >"${workdir}/terminal.json"
python3 - "${workdir}" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
assert len(json.loads((root/"validate.json").read_text())["plan"]["packages"]) >= 8
preview=json.loads((root/"preview.json").read_text())
assert preview["members"][0]["compilation"]["validation"]["valid"]
terminal=json.loads((root/"terminal.json").read_text())
assert terminal["run"]["status"] == "completed", terminal["run"]["status"]
assert terminal["terminal_output"]["node_id"] == "operator_decision"
nodes={a["node_id"] for a in terminal["activations"]}
assert {"plan","implement","validation","review","checkpoint","synchronize","completion","operator_decision"} <= nodes
assert len(terminal["descendant_runs"]) >= 7
PY
stop_server
echo "release-delivery-workflow-package: PASS"
