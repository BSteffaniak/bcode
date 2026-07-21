#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d /tmp/bcode-smoke.XXXXXX)"
server_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

case "$(uname -s)" in
    Darwin|Linux) ;;
    *)
        echo "smoke-tui-pty: SKIP (PTY acceptance requires Darwin or Linux)"
        exit 0
        ;;
esac

cd "${root}"

cargo build --quiet -p bcode --features app

mkdir -p "${workdir}/tmp"
export TMPDIR="${workdir}/tmp"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_STATE_DIR="${workdir}/state"
export BCODE_NO_ONBOARD=1
cat >"${BCODE_CONFIG}" <<'EOF'
[plugins]
enabled = []
EOF

"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
if ! "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
    echo "isolated daemon did not become ready" >&2
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi

session_id="$("${root}/target/debug/bcode" session create tui-pty-smoke)"
python3 - "${root}/target/debug/bcode" "${session_id}" "${workdir}/tui.capture" <<'PY'
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

binary, session_id, capture_path = sys.argv[1:]
session_marker = f"#{session_id[:8]}".encode()
pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "tui", session_id])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
capture = bytearray()
deadline = time.monotonic() + 15
exit_status = None
exit_requested = False

while time.monotonic() < deadline:
    readable, _, _ = select.select([fd], [], [], 0.2)
    if readable:
        try:
            chunk = os.read(fd, 65_536)
        except OSError:
            chunk = b""
        if not chunk:
            break
        capture.extend(chunk)

    if not exit_requested and b"bcode" in capture and session_marker in capture:
        try:
            os.write(fd, b"\x04")
        except OSError:
            pass
        exit_requested = True

    waited_pid, status = os.waitpid(pid, os.WNOHANG)
    if waited_pid:
        exit_status = status
        break

if exit_status is None:
    try:
        os.write(fd, b"\x04")
    except OSError:
        pass
    time.sleep(0.5)
    waited_pid, status = os.waitpid(pid, os.WNOHANG)
    if waited_pid:
        exit_status = status

if exit_status is None:
    os.kill(pid, signal.SIGKILL)
    time.sleep(0.1)
    waited_pid, status = os.waitpid(pid, os.WNOHANG)
    exit_status = status if waited_pid else signal.SIGKILL

with open(capture_path, "wb") as capture_file:
    capture_file.write(capture)

checks = {
    "alternate-screen entry": b"\x1b[?1049h" in capture,
    "alternate-screen restoration": b"\x1b[?1049l" in capture,
    "bracketed-paste entry": b"\x1b[?2004h" in capture,
    "bracketed-paste restoration": b"\x1b[?2004l" in capture,
    "rendered Bcode frame": b"bcode" in capture,
    "rendered session identity": session_marker in capture,
    "rendered provider status": b"provider" in capture,
    "clean Ctrl-D exit": os.WIFEXITED(exit_status) and os.WEXITSTATUS(exit_status) == 0,
}
failures = [name for name, passed in checks.items() if not passed]
if failures:
    print("TUI PTY acceptance failed: " + ", ".join(failures), file=sys.stderr)
    print(repr(bytes(capture[-2_000:])), file=sys.stderr)
    sys.exit(1)
PY

"${root}/target/debug/bcode" server status >/dev/null
"${root}/target/debug/bcode" server stop >/dev/null
wait "${server_pid}"
server_pid=""

echo "smoke-tui-pty: PASS"
