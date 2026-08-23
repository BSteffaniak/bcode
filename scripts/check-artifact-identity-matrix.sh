#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-artifact-identity.XXXXXX)"
cleanup() {
    if command -v pgrep >/dev/null 2>&1; then
        while read -r daemon_pid; do
            [[ -n "${daemon_pid}" ]] && kill "${daemon_pid}" 2>/dev/null || true
        done < <(pgrep -f "${workdir}/.*/daemon-images" || true)
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

cd "${root}"

count_live_record_processes() {
    local state_dir="$1"
    python3 - "${state_dir}" <<'PY'
import json
import os
import pathlib
import sys

count = 0
for path in pathlib.Path(sys.argv[1], "daemons").glob("*.json"):
    with path.open("r", encoding="utf-8") as record_file:
        pid = json.load(record_file).get("pid")
    if isinstance(pid, int):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        except PermissionError:
            pass
        count += 1
print(count)
PY
}

build_artifact() {
    local name="$1"
    local artifact_id="$2"
    local features="$3"
    local profile_flag="$4"
    local target_dir="${CARGO_TARGET_DIR:-${root}/target}"

    BCODE_ARTIFACT_ID="${artifact_id}" CARGO_TARGET_DIR="${target_dir}" \
        cargo build --quiet --package bcode --bin bcode --no-default-features \
        --features "${features}" ${profile_flag}

    local profile="debug"
    if [[ "${profile_flag}" == "--release" ]]; then
        profile="release"
    fi
    cp "${target_dir}/${profile}/bcode" "${workdir}/${name}"
}

build_artifact debug-a matrix-debug-a app ""
build_artifact release-b matrix-release-b app --release
build_artifact feature-c matrix-feature-c app,web-renderer ""

check_artifact() {
    local binary="$1"
    local expected="$2"
    local state_dir="$3"
    local actual

    actual="$("${binary}" artifact-id)"
    if [[ "${actual}" != "${expected}" ]]; then
        echo "artifact identity mismatch for ${binary}: expected ${expected}, found ${actual}" >&2
        exit 1
    fi

    BCODE_STATE_DIR="${state_dir}" "${binary}" server start >/dev/null
    check_running_artifact "${binary}" "${expected}" "${state_dir}"
    BCODE_STATE_DIR="${state_dir}" "${binary}" server stop >/dev/null
}

check_running_artifact() {
    local binary="$1"
    local expected="$2"
    local state_dir="$3"
    local status

    status="$(BCODE_STATE_DIR="${state_dir}" "${binary}" server status --verbose)"
    if ! grep -q "^artifact identity: ${expected}$" <<<"${status}"; then
        echo "daemon did not report the expected artifact identity for ${binary}" >&2
        printf '%s\n' "${status}" >&2
        exit 1
    fi
}

for name in debug-a release-b feature-c; do
    check_artifact "${workdir}/${name}" "matrix-${name}" "${workdir}/${name}-state"
done

if [[ "$("${workdir}/debug-a" artifact-id)" == "$("${workdir}/release-b" artifact-id)" ]] || \
   [[ "$("${workdir}/debug-a" artifact-id)" == "$("${workdir}/feature-c" artifact-id)" ]]; then
    echo "separately produced artifacts unexpectedly share identity" >&2
    exit 1
fi

cp "${workdir}/debug-a" "${workdir}/copied-debug-a"
check_artifact "${workdir}/copied-debug-a" "matrix-debug-a" "${workdir}/copied-debug-a-state"

coexist_state="${workdir}/coexist-state"
BCODE_STATE_DIR="${coexist_state}" "${workdir}/debug-a" server start >/dev/null
BCODE_STATE_DIR="${coexist_state}" "${workdir}/release-b" server start >/dev/null
check_running_artifact "${workdir}/debug-a" matrix-debug-a "${coexist_state}"
check_running_artifact "${workdir}/release-b" matrix-release-b "${coexist_state}"
record_count="$(find "${coexist_state}/daemons" -name '*.json' -type f | wc -l | tr -d ' ')"
if [[ "${record_count}" != "2" ]]; then
    echo "expected two coexisting exact-artifact daemon records, found ${record_count}" >&2
    exit 1
fi
BCODE_STATE_DIR="${coexist_state}" "${workdir}/debug-a" server stop >/dev/null
check_running_artifact "${workdir}/release-b" matrix-release-b "${coexist_state}"
BCODE_STATE_DIR="${coexist_state}" "${workdir}/release-b" server stop >/dev/null

session_state="${workdir}/session-state"
session_root="${workdir}/canonical-sessions"
BCODE_STATE_DIR="${session_state}" BCODE_SESSION_STORE_DIR="${session_root}" \
    "${workdir}/debug-a" server start >/dev/null
session_id="$(BCODE_STATE_DIR="${session_state}" BCODE_SESSION_STORE_DIR="${session_root}" \
    "${workdir}/debug-a" session create cross-artifact-session)"

# BCODE_SESSION_STORE_DIR must actually relocate canonical session storage. Without
# this assertion the surrounding cross-artifact ownership checks would still pass
# while every session silently lived under the default <state>/sessions root.
if [[ ! -f "${session_root}/${session_id}/session.db" ]]; then
    echo "canonical session storage did not honor BCODE_SESSION_STORE_DIR" >&2
    echo "expected ${session_root}/${session_id}/session.db" >&2
    find "${session_root}" "${session_state}" -name 'session.db' -print >&2 2>/dev/null || true
    exit 1
fi
if [[ -e "${session_state}/sessions/${session_id}" ]]; then
    echo "session storage leaked into the state-dir default root despite an explicit session root" >&2
    exit 1
fi
BCODE_STATE_DIR="${session_state}" BCODE_SESSION_STORE_DIR="${session_root}" \
    "${workdir}/debug-a" attach "${session_id}" >"${workdir}/session-owner-attach.log" 2>&1 &
owner_attach_pid="$!"
sleep 0.5
if BCODE_STATE_DIR="${session_state}" BCODE_SESSION_STORE_DIR="${session_root}" \
    "${workdir}/release-b" attach "${session_id}" >"${workdir}/foreign-attach.log" 2>&1; then
    echo "foreign artifact unexpectedly acquired a live-owned session" >&2
    exit 1
fi
if ! grep -Eq 'active elsewhere|another incompatible Bcode writer|session_active_elsewhere' "${workdir}/foreign-attach.log"; then
    echo "foreign artifact ownership rejection was not actionable" >&2
    cat "${workdir}/foreign-attach.log" >&2
    exit 1
fi
kill "${owner_attach_pid}" 2>/dev/null || true
wait "${owner_attach_pid}" 2>/dev/null || true
released_attach_pid=""
for attempt in {1..100}; do
    released_log="${workdir}/released-attach-${attempt}.log"
    BCODE_STATE_DIR="${session_state}" BCODE_SESSION_STORE_DIR="${session_root}" \
        "${workdir}/release-b" attach "${session_id}" >"${released_log}" 2>&1 &
    candidate_pid="$!"
    sleep 0.1
    if kill -0 "${candidate_pid}" 2>/dev/null; then
        released_attach_pid="${candidate_pid}"
        break
    fi
    wait "${candidate_pid}" 2>/dev/null || true
    if ! grep -Eq 'active elsewhere|another incompatible Bcode writer|session_active_elsewhere' "${released_log}"; then
        echo "foreign artifact ownership acquisition failed unexpectedly" >&2
        cat "${released_log}" >&2
        exit 1
    fi
done
if [[ -z "${released_attach_pid}" ]]; then
    echo "foreign artifact could not acquire released canonical session ownership" >&2
    exit 1
fi
kill "${released_attach_pid}" 2>/dev/null || true
wait "${released_attach_pid}" 2>/dev/null || true
BCODE_STATE_DIR="${session_state}" BCODE_SESSION_STORE_DIR="${session_root}" \
    "${workdir}/debug-a" server stop >/dev/null
BCODE_STATE_DIR="${session_state}" BCODE_SESSION_STORE_DIR="${session_root}" \
    "${workdir}/release-b" server stop >/dev/null

concurrent_state="${workdir}/concurrent-state"
client_pids=()
for client in {1..16}; do
    BCODE_STATE_DIR="${concurrent_state}" "${workdir}/debug-a" server start \
        >"${workdir}/concurrent-client-${client}.log" 2>&1 &
    client_pids+=("$!")
done
for client_index in "${!client_pids[@]}"; do
    if ! wait "${client_pids[${client_index}]}"; then
        echo "concurrent client $((client_index + 1)) failed" >&2
        cat "${workdir}/concurrent-client-$((client_index + 1)).log" >&2
        exit 1
    fi
done
check_running_artifact "${workdir}/debug-a" matrix-debug-a "${concurrent_state}"
record_count="$(find "${concurrent_state}/daemons" -name '*.json' -type f | wc -l | tr -d ' ')"
process_count="$(count_live_record_processes "${concurrent_state}")"
if [[ "${record_count}" != "1" || "${process_count}" != "1" ]]; then
    echo "concurrent clients produced ${record_count} records and ${process_count} daemon processes" >&2
    exit 1
fi
BCODE_STATE_DIR="${concurrent_state}" "${workdir}/debug-a" server stop >/dev/null

restart_state="${workdir}/restart-state"
BCODE_STATE_DIR="${restart_state}" "${workdir}/debug-a" server start >/dev/null
record_path="$(find "${restart_state}/daemons" -name '*.json' -type f -print -quit)"
old_pid="$(python3 - "${record_path}" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as record_file:
    print(json.load(record_file)["pid"])
PY
)"
kill -9 "${old_pid}"
for _ in {1..100}; do
    if ! kill -0 "${old_pid}" 2>/dev/null; then
        break
    fi
    sleep 0.01
done
BCODE_STATE_DIR="${restart_state}" "${workdir}/debug-a" server start >/dev/null
check_running_artifact "${workdir}/debug-a" matrix-debug-a "${restart_state}"
new_pid="$(python3 - "${record_path}" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as record_file:
    print(json.load(record_file)["pid"])
PY
)"
if [[ "${new_pid}" == "${old_pid}" ]] || ! kill -0 "${new_pid}" 2>/dev/null; then
    echo "daemon crash reconnect did not start one live replacement" >&2
    exit 1
fi
record_count="$(find "${restart_state}/daemons" -name '*.json' -type f | wc -l | tr -d ' ')"
process_count="$(count_live_record_processes "${restart_state}")"
if [[ "${record_count}" != "1" || "${process_count}" != "1" ]]; then
    echo "daemon crash reconnect produced ${record_count} records and ${process_count} processes" >&2
    exit 1
fi
BCODE_STATE_DIR="${restart_state}" "${workdir}/debug-a" server stop >/dev/null

if BCODE_ARTIFACT_ID='invalid/id' CARGO_TARGET_DIR="${workdir}/invalid-target" \
    cargo check --quiet --package bcode_ipc >"${workdir}/invalid.stdout" 2>"${workdir}/invalid.stderr"; then
    echo "malformed artifact identity unexpectedly built" >&2
    exit 1
fi
if ! grep -q 'invalid BCODE_ARTIFACT_ID' "${workdir}/invalid.stderr"; then
    echo "malformed artifact identity did not fail with the expected diagnostic" >&2
    cat "${workdir}/invalid.stderr" >&2
    exit 1
fi

echo "check-artifact-identity-matrix: PASS"
