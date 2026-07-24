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

cargo build --quiet -p bcode --features app -p bcode_fake_provider_plugin

case "$(uname -s)" in
    Darwin)
        fake_dylib="${root}/target/debug/libbcode_fake_provider_plugin.dylib"
        ;;
    Linux)
        fake_dylib="${root}/target/debug/libbcode_fake_provider_plugin.so"
        ;;
esac

mkdir -p "${workdir}/tmp" "${workdir}/config/bcode/plugins/fake-provider"
export TMPDIR="${workdir}/tmp"
export XDG_CONFIG_HOME="${workdir}/config"
export BCODE_CONFIG="${workdir}/bcode.toml"
export BCODE_STATE_DIR="${workdir}/state"
export BCODE_NO_ONBOARD=1
cat >"${workdir}/config/bcode/plugins/fake-provider/bcode-plugin.toml" <<EOF
id = "bcode.fake-provider"
name = "Bcode Fake Model Provider"
version = "0.0.1"

[[services]]
interface_id = "bcode.model-provider/v1"
name = "Fake Model Provider"

[runtime]
type = "native"
abi_version = 2
library = "${fake_dylib}"
event_symbol = "bcode_plugin_handle_event_v1"
service_symbol = "bcode_plugin_invoke_service_v1"
EOF
cat >"${BCODE_CONFIG}" <<'EOF'
[plugins]
enabled = ["bcode.fake-provider", "bcode.shell"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[model.prompt_cache]
mode = "off"

[agent.build.permission]
command = { "*" = "allow" }

[tools.shell.env]
mode = "inherit"
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
import subprocess
import sys
import tempfile
import termios
import time

binary, session_id, capture_path = sys.argv[1:]
session_marker = f"#{session_id[:8]}".encode()
pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "tui", session_id])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
capture = bytearray()
deadline = time.monotonic() + 120
exit_status = None
exit_requested = False
request_sent = False
live_seen_before_finish = False
final_seen_before_live = False
next_screen_probe = 0.0
live_marker = b"FRESH_LIVE_OUTPUT"
final_marker = b"FRESH_FINAL_OUTPUT"
probe_source = r'''
use bmux_terminal_grid::{GridLimits, TerminalGridStream, visible_text};
use std::io::Read as _;
fn main() {
    let path = std::env::args().nth(1).expect("capture path");
    let mut bytes = Vec::new();
    std::fs::File::open(path).unwrap().read_to_end(&mut bytes).unwrap();
    let mut stream = TerminalGridStream::new(120, 30, GridLimits::default()).unwrap();
    stream.process(&bytes);
    print!("{}", visible_text(stream.grid(), 0, 30));
}
'''
probe_dir = tempfile.mkdtemp(prefix="bcode-tui-grid-")
with open(os.path.join(probe_dir, "Cargo.toml"), "w", encoding="utf-8") as manifest:
    manifest.write('''[package]\nname="bcode-tui-grid-probe"\nversion="0.0.0"\nedition="2024"\n[dependencies]\nbmux_terminal_grid={git="https://github.com/BSteffaniak/bmux.git",rev="44a7249f"}\n''')
os.mkdir(os.path.join(probe_dir, "src"))
with open(os.path.join(probe_dir, "src", "main.rs"), "w", encoding="utf-8") as source:
    source.write(probe_source)
subprocess.run(
    ["cargo", "build", "--quiet", "--manifest-path", os.path.join(probe_dir, "Cargo.toml")],
    check=True,
)
probe_binary = os.path.join(probe_dir, "target", "debug", "bcode-tui-grid-probe")

def screen_text():
    with open(capture_path, "wb") as capture_file:
        capture_file.write(capture)
    result = subprocess.run(
        [probe_binary, capture_path],
        check=True,
        capture_output=True,
    )
    return result.stdout

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

    if not request_sent and b"bcode" in capture and session_marker in capture:
        os.write(
            fd,
            b"tool-shell printf '\\106\\122\\105\\123\\110\\137\\114\\111\\126\\105\\137\\117\\125\\124\\120\\125\\124\\012'; sleep 4; printf '\\106\\122\\105\\123\\110\\137\\106\\111\\116\\101\\114\\137\\117\\125\\124\\120\\125\\124\\012'\r",
        )
        request_sent = True

    if request_sent and time.monotonic() >= next_screen_probe:
        screen = screen_text()
        if live_marker in screen and not live_seen_before_finish:
            live_seen_before_finish = final_marker not in screen
        if final_marker in screen and live_marker not in screen:
            final_seen_before_live = True
        if live_seen_before_finish and final_marker in screen:
            capture.extend(b"\nBCODE_SMOKE_FINAL_MARKER_VISIBLE\n")
        next_screen_probe = time.monotonic() + 0.25

    if (
        not exit_requested
        and request_sent
        and live_seen_before_finish
        and b"BCODE_SMOKE_FINAL_MARKER_VISIBLE" in capture
    ):
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
    "shell request sent": request_sent,
    "live output visible before command completion": live_seen_before_finish,
    "final output did not precede live output": not final_seen_before_live,
    "final output visible after command completion": b"BCODE_SMOKE_FINAL_MARKER_VISIBLE"
    in capture,
    "clean Ctrl-D exit": os.WIFEXITED(exit_status) and os.WEXITSTATUS(exit_status) == 0,
}
failures = [name for name, passed in checks.items() if not passed]
if failures:
    print("TUI PTY acceptance failed: " + ", ".join(failures), file=sys.stderr)
    subprocess.run([binary, "session", "history", session_id], check=False)
    try:
        with open(os.path.join(os.environ["BCODE_STATE_DIR"], "..", "server.log"), "r", encoding="utf-8") as server_log:
            print(server_log.read(), file=sys.stderr)
    except OSError:
        pass
    print(screen_text().decode(errors="replace"), file=sys.stderr)
    print(repr(bytes(capture[-2_000:])), file=sys.stderr)
    sys.exit(1)
PY

"${root}/target/debug/bcode" server status >/dev/null
"${root}/target/debug/bcode" server stop >/dev/null
wait "${server_pid}" || true
server_pid=""

echo "smoke-tui-pty: PASS"
