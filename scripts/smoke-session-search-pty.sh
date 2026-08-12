#!/usr/bin/env bash
set -euo pipefail

unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile="${BCODE_SESSION_SEARCH_PTY_PROFILE:-debug}"
case "${profile}" in
    debug)
        cargo_profile_args=()
        target_profile="debug"
        ;;
    release)
        cargo_profile_args=(--release)
        target_profile="release"
        ;;
    *)
        echo "smoke-session-search-pty: profile must be debug or release" >&2
        exit 2
        ;;
esac

case "$(uname -s)" in
    Darwin|Linux) ;;
    *)
        echo "smoke-session-search-pty: SKIP (PTY acceptance requires Darwin or Linux)"
        exit 0
        ;;
esac

bcode_binary="${root}/target/${target_profile}/bcode"
grid_probe_binary="${root}/target/${target_profile}/bcode_terminal_grid_probe"
workdir="$(cd "$(mktemp -d /tmp/bcode-search-pty.XXXXXX)" && pwd -P)"
server_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    if [[ "${BCODE_SESSION_SEARCH_PTY_KEEP_WORKDIR:-0}" == "1" ]]; then
        echo "smoke-session-search-pty: retained ${workdir}" >&2
    else
        rm -rf "${workdir}"
    fi
}
trap cleanup EXIT

cd "${root}"
if [[ "${BCODE_SESSION_SEARCH_PTY_SKIP_BUILD:-0}" != "1" ]]; then
    cargo build "${cargo_profile_args[@]}" --quiet -p bcode --features distribution
    cargo build "${cargo_profile_args[@]}" --quiet -p bcode_tui_components --features terminal-viewer --bin bcode_terminal_grid_probe
fi
if [[ ! -x "${bcode_binary}" || ! -x "${grid_probe_binary}" ]]; then
    echo "smoke-session-search-pty: required binaries are missing" >&2
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
    if grep -q "server ready; accepting clients" "${workdir}/server.log" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if ! grep -q "server ready; accepting clients" "${workdir}/server.log" 2>/dev/null; then
    echo "smoke-session-search-pty: isolated daemon did not become ready" >&2
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi

marker="${BCODE_SESSION_SEARCH_PTY_MARKER:-PTYSESSIONSEARCHMARKER}"
fixture_sessions="${BCODE_SESSION_SEARCH_PTY_FIXTURE_SESSIONS:-1}"
if [[ ! "${fixture_sessions}" =~ ^[1-9][0-9]*$ ]]; then
    echo "smoke-session-search-pty: fixture session count must be a positive integer" >&2
    exit 2
fi
session_id=""
for index in $(seq 1 "${fixture_sessions}"); do
    session_name="session-search-pty"
    message="${marker} canonical transcript"
    if (( fixture_sessions > 1 )); then
        session_name="${marker} page-${index}"
        message="pagination fixture ${index}"
    fi
    created="$(cd "${workdir}" && "${bcode_binary}" session create "${session_name}")"
    if [[ -z "${session_id}" ]]; then
        session_id="${created}"
    fi
    "${bcode_binary}" send "${created}" "${message}" >/dev/null
done
for _ in {1..100}; do
    if "${bcode_binary}" session search "${marker}" --json \
        | python3 -c 'import json, sys; raise SystemExit(0 if json.load(sys.stdin)["hits"] else 1)' 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if ! "${bcode_binary}" session search "${marker}" --json \
    | python3 -c 'import json, sys; raise SystemExit(0 if json.load(sys.stdin)["hits"] else 1)'; then
    echo "smoke-session-search-pty: incremental search indexing did not become ready" >&2
    exit 1
fi

python3 - "${bcode_binary}" "${grid_probe_binary}" "${session_id}" "${marker}" "${workdir}/tui.capture" <<'PY'
import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

binary, probe_binary, session_id, marker, capture_path = sys.argv[1:]
fixture_sessions = int(os.environ.get("BCODE_SESSION_SEARCH_PTY_FIXTURE_SESSIONS", "1"))
marker_bytes = marker.encode()
session_marker = f"#{session_id[:8]}".encode()
pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "tui", session_id])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
capture = bytearray()
opened_search = False
search_attempted_at = None
submitted_query = False
result_visible = False
next_page_started = False
next_page_completed = fixture_sessions == 1
inventory_started = False
inventory_completed = False
backfill_started = False
backfill_completed = False
selected_result = False
navigated = False
exit_requested = False
exit_status = None
deadline = time.monotonic() + int(os.environ.get("BCODE_SESSION_SEARCH_PTY_TIMEOUT_SECS", "45"))
next_probe = 0.0
last_screen = b""

def screen_text():
    with open(capture_path, "wb") as capture_file:
        capture_file.write(capture)
    return subprocess.run(
        [probe_binary, capture_path],
        check=True,
        capture_output=True,
    ).stdout

while time.monotonic() < deadline:
    readable, _, _ = select.select([fd], [], [], 0.05)
    if readable:
        try:
            chunk = os.read(fd, 65_536)
        except OSError:
            chunk = b""
        if not chunk:
            break
        capture.extend(chunk)

    if time.monotonic() < next_probe:
        continue
    next_probe = time.monotonic() + 0.1
    last_screen = screen_text()
    lower = last_screen.lower()

    if not opened_search and session_marker in last_screen and b"ready" in lower:
        time.sleep(0.5)
        os.write(fd, b"/search\r")
        opened_search = True
        search_attempted_at = time.monotonic()
        continue
    if opened_search and not submitted_query and b"transcript query" not in lower:
        if search_attempted_at is not None and time.monotonic() - search_attempted_at > 1:
            os.write(fd, b"/search\r")
            search_attempted_at = time.monotonic()
        continue
    if opened_search and not submitted_query and b"transcript query" in lower:
        query = marker_bytes
        if fixture_sessions > 1:
            query = b"content:title " + marker_bytes
        os.write(fd, query + b"\r")
        submitted_query = True
        continue
    if submitted_query and not next_page_started and marker_bytes in last_screen:
        if b"query complete" not in lower or b"results from" not in lower:
            continue
        result_visible = True
        if fixture_sessions > 1:
            os.write(fd, b"\x1b[110;3u")
            next_page_started = True
            continue
        next_page_started = True
    if next_page_started and not next_page_completed and b"1 results from" in lower:
        next_page_completed = True
    if submitted_query and next_page_completed and not inventory_started:
        os.write(fd, b"\x1b[105;3u")
        inventory_started = True
        continue
    if inventory_started and not inventory_completed and (
        b"canonical migration completed" in lower
        or b"canonical migration needs attention" in lower
    ):
        inventory_completed = True
        os.write(fd, b"\x1b[98;3u")
        backfill_started = True
        continue
    if backfill_started and not backfill_completed and b"derived search backfill completed" in lower:
        backfill_completed = True
        os.write(fd, b"\r")
        selected_result = True
        continue
    if (
        selected_result
        and b"ask bcode" in lower
        and marker_bytes in last_screen
    ):
        navigated = True
        exit_requested = True
        break

    waited_pid, status = os.waitpid(pid, os.WNOHANG)
    if waited_pid == pid:
        exit_status = status
        break

while True:
    readable, _, _ = select.select([fd], [], [], 0)
    if not readable:
        break
    try:
        chunk = os.read(fd, 65_536)
    except OSError:
        break
    if not chunk:
        break
    capture.extend(chunk)
last_screen = screen_text()
if selected_result and marker_bytes in last_screen:
    navigated = True

if exit_status is None:
    try:
        os.write(fd, b"\x04")
    except OSError:
        pass
    end = time.monotonic() + 3
    while time.monotonic() < end:
        waited_pid, status = os.waitpid(pid, os.WNOHANG)
        if waited_pid == pid:
            exit_status = status
            break
        time.sleep(0.05)
if exit_status is None:
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
    end = time.monotonic() + 1
    while time.monotonic() < end:
        waited_pid, status = os.waitpid(pid, os.WNOHANG)
        if waited_pid == pid:
            exit_status = status
            break
        time.sleep(0.05)
    if exit_status is None:
        exit_status = signal.SIGKILL

checks = {
    "search route opened": opened_search and submitted_query,
    "ordinary result rendered": result_visible,
    "next result page completed from TUI": next_page_completed,
    "compatibility inventory completed from TUI": inventory_completed,
    "all-provider backfill completed from TUI": backfill_completed,
    "selected result navigated to canonical transcript": navigated,
}
failures = [name for name, passed in checks.items() if not passed]
if failures:
    print("session-search PTY acceptance failed: " + ", ".join(failures), file=sys.stderr)
    print(f"state: opened={opened_search} submitted={submitted_query} result={result_visible} next_page={next_page_completed} inventory={inventory_completed} backfill={backfill_completed} selected={selected_result} navigated={navigated}", file=sys.stderr)
    print(last_screen.decode(errors="replace"), file=sys.stderr)
    sys.exit(1)
PY

kill "${server_pid}" 2>/dev/null || true
wait "${server_pid}" 2>/dev/null || true
server_pid=""
echo "smoke-session-search-pty: PASS"
