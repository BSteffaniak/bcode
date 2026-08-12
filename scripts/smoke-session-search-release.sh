#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bcode_binary="${root}/target/release/bcode"
workdir="$(cd "$(mktemp -d /tmp/bcode-release-search.XXXXXX)" && pwd -P)"
server_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    if [[ "${BCODE_SESSION_SEARCH_RELEASE_KEEP_WORKDIR:-0}" == "1" ]]; then
        echo "smoke-session-search-release: retained ${workdir}" >&2
    else
        rm -rf "${workdir}"
    fi
}
trap cleanup EXIT

if [[ ! -x "${bcode_binary}" ]]; then
    echo "smoke-session-search-release: current release binary is missing" >&2
    exit 1
fi

mkdir -p "${workdir}/config" "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_STATE_DIR="${workdir}/state"
export BCODE_NO_ONBOARD=1
cat >"${BCODE_CONFIG}" <<'EOF'
[plugins]
default = "all"

[daemon]
idle_shutdown = false
EOF

"${bcode_binary}" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..600}; do
    if ! kill -0 "${server_pid}" 2>/dev/null; then
        wait "${server_pid}" || true
        cat "${workdir}/server.log" >&2 || true
        echo "smoke-session-search-release: isolated daemon exited before readiness" >&2
        exit 1
    fi
    if "${bcode_binary}" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
if ! "${bcode_binary}" server status >/dev/null 2>&1; then
    cat "${workdir}/server.log" >&2 || true
    echo "smoke-session-search-release: isolated daemon did not become ready" >&2
    exit 1
fi

session_id="$(cd "${workdir}" && "${bcode_binary}" session create release-routing)"
"${bcode_binary}" send "${session_id}" RELEASEORDINARYMARKER >/dev/null
for _ in {1..100}; do
    if "${bcode_binary}" session search RELEASEORDINARYMARKER --json >"${workdir}/ordinary.json" \
        && python3 - "${workdir}/ordinary.json" <<'PY'
import json
import sys
raise SystemExit(0 if json.load(open(sys.argv[1]))["hits"] else 1)
PY
    then
        break
    fi
    sleep 0.1
done

"${bcode_binary}" session search RELEASEORDINARYMARKER --json >"${workdir}/ordinary.json"
"${bcode_binary}" session search RELEASEORDINARYMARKER --deep \
    --content shell-output --content tool-output --json >"${workdir}/deep.json"
"${bcode_binary}" session search-status --json >"${workdir}/status.json"
"${bcode_binary}" session migrate-inventory --json >"${workdir}/inventory.json"
"${bcode_binary}" session migrate-start --confirm migrate-supported-sessions --foreground --json \
    >"${workdir}/migration.json"
"${bcode_binary}" session search-backfill-start --deadline-ms 30000 --json \
    >"${workdir}/backfill-start.json"
backfill_operation_id="$(python3 - "${workdir}/backfill-start.json" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1]))["operation_id"])
PY
)"
migration_operation_id="$(python3 - "${workdir}/migration.json" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1]))["operation_id"])
PY
)"
"${bcode_binary}" session search-backfill-cancel "${backfill_operation_id}" --json \
    >"${workdir}/backfill-cancel.json"

python3 - "${workdir}/ordinary.json" "${workdir}/deep.json" "${workdir}/status.json" \
    "${workdir}/inventory.json" "${workdir}/migration.json" "${workdir}/backfill-cancel.json" <<'PY'
import json
import sys

ordinary, deep, status, inventory, migration, cancellation = (
    json.load(open(path)) for path in sys.argv[1:]
)
assert ordinary["hits"], ordinary
ordinary_ids = {provider["provider_id"] for provider in ordinary["providers"]}
deep_ids = {provider["provider_id"] for provider in deep["providers"]}
assert "bcode.tantivy-session-search" in ordinary_ids, ordinary_ids
assert "bcode.compressed-session-search" not in ordinary_ids, ordinary_ids
assert "bcode.compressed-session-search" in deep_ids, deep_ids
provider_ids = {provider["plugin_id"] for provider in status["providers"]}
assert {
    "bcode.tantivy-session-search",
    "bcode.compressed-session-search",
} <= provider_ids, provider_ids
assert inventory["mode"] == "inventory" and inventory["state"] == "completed", inventory
assert inventory["visited"] >= 1 and inventory["failed"] == 0, inventory
assert migration["state"] == "completed", migration
assert cancellation["state"] in {"cancellation_requested", "cancelled", "completed"}, cancellation
PY

kill "${server_pid}" 2>/dev/null || true
wait "${server_pid}" 2>/dev/null || true
server_pid=""
"${bcode_binary}" server run >"${workdir}/restart-server.log" 2>&1 &
server_pid="$!"
for _ in {1..600}; do
    if ! kill -0 "${server_pid}" 2>/dev/null; then
        wait "${server_pid}" || true
        cat "${workdir}/restart-server.log" >&2 || true
        echo "smoke-session-search-release: restarted daemon exited before readiness" >&2
        exit 1
    fi
    if "${bcode_binary}" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
if "${bcode_binary}" session migrate-status "${migration_operation_id}" --json \
    >"${workdir}/stale-migration.out" 2>"${workdir}/stale-migration.err"; then
    echo "smoke-session-search-release: transient migration survived restart" >&2
    exit 1
fi
if "${bcode_binary}" session search-backfill-status "${backfill_operation_id}" --json \
    >"${workdir}/stale-backfill.out" 2>"${workdir}/stale-backfill.err"; then
    echo "smoke-session-search-release: transient backfill survived restart" >&2
    exit 1
fi
grep -q "aggregate operation state is transient" "${workdir}/stale-migration.err"
grep -q "unknown session-search backfill operation" "${workdir}/stale-backfill.err"
"${bcode_binary}" session migrate-start --confirm migrate-supported-sessions --foreground --json \
    >"${workdir}/migration-after-restart.json"
"${bcode_binary}" session search-backfill --deadline-ms 30000 --json \
    >"${workdir}/backfill-after-restart.json"
python3 - "${workdir}/migration-after-restart.json" \
    "${workdir}/backfill-after-restart.json" <<'PY'
import json
import sys

migration, backfill = (json.load(open(path)) for path in sys.argv[1:])
assert migration["state"] == "completed", migration
assert migration["selected"] == 0 and migration["failed"] == 0, migration
assert backfill["state"] == "completed", backfill
response = backfill["complete_response"]
assert response["convergence_passes"] >= 1 and not response["cancelled"], response
assert all(provider["failed_sessions"] == 0 for provider in response["providers"]), response
PY

kill "${server_pid}" 2>/dev/null || true
wait "${server_pid}" 2>/dev/null || true
server_pid=""
cat >"${BCODE_CONFIG}" <<'EOF'
[plugins]
default = "all"

[session_search]
enabled = false

[daemon]
idle_shutdown = false
EOF
"${bcode_binary}" server run >"${workdir}/disabled-server.log" 2>&1 &
server_pid="$!"
for _ in {1..600}; do
    if ! kill -0 "${server_pid}" 2>/dev/null; then
        wait "${server_pid}" || true
        cat "${workdir}/disabled-server.log" >&2 || true
        echo "smoke-session-search-release: disabled-search daemon exited before readiness" >&2
        exit 1
    fi
    if "${bcode_binary}" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
"${bcode_binary}" session list --json >"${workdir}/disabled-list.json"
"${bcode_binary}" session search RELEASEORDINARYMARKER --json >"${workdir}/disabled-search.json"
python3 - "${workdir}/disabled-list.json" "${workdir}/disabled-search.json" <<'PY'
import json
import sys

assert isinstance(json.load(open(sys.argv[1])), list)
search = json.load(open(sys.argv[2]))
assert search["outcome"] == "no_eligible_provider", search
assert not search["providers"] and not search["hits"], search
assert not search["query_complete"] and not search["coverage_complete"], search
PY

echo "smoke-session-search-release: PASS"
