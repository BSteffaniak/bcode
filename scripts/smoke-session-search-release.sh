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
python3 - "${workdir}/ordinary.json" "${workdir}/deep.json" "${workdir}/status.json" <<'PY'
import json
import sys

ordinary, deep, status = (json.load(open(path)) for path in sys.argv[1:])
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
PY

echo "smoke-session-search-release: PASS"
