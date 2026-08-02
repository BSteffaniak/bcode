#!/usr/bin/env bash
set -euo pipefail

# Smoke tests own isolated process state and must not inherit the invoking daemon.
unset BCODE_DAEMON_LOG BCODE_IPC_ENDPOINT BCODE_IPC_ENDPOINT_NAMESPACE

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(cd "$(mktemp -d /tmp/bcode-smoke.XXXXXX)" && pwd -P)"
server_pid=""
cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
    if [[ "${BCODE_TUI_PTY_KEEP_WORKDIR:-0}" == "1" ]]; then
        echo "smoke-tui-pty: retained ${workdir}" >&2
    else
        rm -rf "${workdir}"
    fi
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

if [[ "${BCODE_TUI_PTY_SKIP_BUILD:-0}" != "1" ]]; then
    cargo build --quiet -p bcode --features distribution -p bcode_fake_provider_plugin
    cargo build --quiet -p bcode_tui_components --bin bcode_terminal_grid_probe
fi

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
profile = "pty-smoke"

[model.profiles.pty-smoke]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[model.profiles.pty-smoke.settings]
fake_tool_delta_delay_ms = "500"

[model.prompt_cache]
mode = "off"

[agent.build.permission]
command = { "*" = "ask" }
write = { "**" = "allow" }
edit = { "**" = "allow" }

[tools.shell.env]
mode = "inherit"

[daemon]
idle_shutdown = true
idle_shutdown_after_secs = 1
EOF

"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..600}; do
    if [[ -s "${workdir}/server.log" ]] && grep -q "server ready; accepting clients" "${workdir}/server.log"; then
        break
    fi
    sleep 0.1
done
if ! grep -q "server ready; accepting clients" "${workdir}/server.log"; then
    echo "isolated daemon did not become ready" >&2
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi

session_id="$(trap - EXIT; cd "${workdir}" && "${root}/target/debug/bcode" session create tui-pty-smoke)"
initial_pid="$(python3 - "${BCODE_STATE_DIR}/daemons" <<'PY'
import glob
import json
import os
import sys
records = glob.glob(os.path.join(sys.argv[1], "*.json"))
with open(records[0], "r", encoding="utf-8") as record_file:
    print(json.load(record_file)["pid"])
PY
)"
wait "${server_pid}"
server_pid=""
for _ in {1..100}; do
    if ! kill -0 "${initial_pid}" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if kill -0 "${initial_pid}" 2>/dev/null; then
    echo "isolated daemon did not retire after configured idle interval" >&2
    exit 1
fi

expected_artifact_id="$("${root}/target/debug/bcode" artifact-id)"

python3 - "${root}/target/debug/bcode" "${session_id}" "${initial_pid}" "${expected_artifact_id}" <<'PY'
import fcntl
import glob
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

binary, session_id, initial_pid, expected_artifact_id = sys.argv[1:]
initial_pid = int(initial_pid)
session_marker = f"#{session_id[:8]}".encode()
started = time.monotonic()
pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "tui", session_id])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
capture = bytearray()
daemon_ready_at = None
connected_at = None
verified_artifact_id = None
request_sent = False
deadline = started + 10
while time.monotonic() < deadline:
    ready, _, _ = select.select([fd], [], [], 0.05)
    if ready:
        try:
            capture.extend(os.read(fd, 65536))
        except OSError:
            break
    if not request_sent and session_marker in capture:
        os.write(fd, b"cold-auto-start-input\r")
        request_sent = True
    if daemon_ready_at is None:
        for record_path in glob.glob(os.path.join(os.environ["BCODE_STATE_DIR"], "daemons", "*.json")):
            try:
                with open(record_path, "r", encoding="utf-8") as record_file:
                    record = json.load(record_file)
                daemon_pid = record.get("pid")
                artifact_id = record.get("artifact_id")
                if daemon_pid and daemon_pid != initial_pid and artifact_id:
                    os.kill(daemon_pid, 0)
                    verified_artifact_id = artifact_id
                    daemon_ready_at = time.monotonic()
                    break
            except (OSError, ValueError):
                pass
    if session_marker in capture and daemon_ready_at is not None:
        connected_at = time.monotonic()
        break

try:
    os.write(fd, b"\x1b\x1b")
except OSError:
    pass
exit_status = None
exit_deadline = time.monotonic() + 3
while time.monotonic() < exit_deadline:
    ready, _, _ = select.select([fd], [], [], 0.05)
    if ready:
        try:
            capture.extend(os.read(fd, 65536))
        except OSError:
            pass
    waited_pid, status = os.waitpid(pid, os.WNOHANG)
    if waited_pid:
        exit_status = status
        break
if exit_status is None:
    os.kill(pid, signal.SIGKILL)
    _, exit_status = os.waitpid(pid, 0)

checks = {
    "rendered session identity": session_marker in capture,
    "TUI auto-started daemon": daemon_ready_at is not None,
    "daemon started within 10 seconds": daemon_ready_at is not None
    and daemon_ready_at - started <= 10,
    "TUI observed auto-started daemon": connected_at is not None,
    "auto-started daemon has exact invoking artifact identity": verified_artifact_id == expected_artifact_id,
    "TUI connected within 10 seconds": connected_at is not None and connected_at - started <= 10,
}
failures = [name for name, passed in checks.items() if not passed]
if failures:
    print("cold TUI auto-start acceptance failed: " + ", ".join(failures), file=sys.stderr)
    print(repr(bytes(capture[-2000:])), file=sys.stderr)
    sys.exit(1)
PY

"${root}/target/debug/bcode" server stop >/dev/null
"${root}/target/debug/bcode" server run >"${workdir}/server.log" 2>&1 &
server_pid="$!"
for _ in {1..100}; do
    if "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
if ! "${root}/target/debug/bcode" server status >/dev/null 2>&1; then
    echo "replacement daemon did not become ready for PTY acceptance" >&2
    cat "${workdir}/server.log" >&2 || true
    exit 1
fi

python3 - "${root}/target/debug/bcode" "${root}/target/debug/bcode_terminal_grid_probe" "${session_id}" "${workdir}/tui.capture" <<'PY'
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

binary, probe_binary, session_id, capture_path = sys.argv[1:]
session_marker = f"#{session_id[:8]}".encode()
pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "tui", session_id])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
capture = bytearray()
deadline = time.monotonic() + int(os.environ.get("BCODE_TUI_PTY_TIMEOUT_SECS", "120"))
exit_deadline = None
exit_status = None
exit_requested = False
shell_request_sent = False
shell_permission_responsive = False
shell_command_visible = False
shell_cwd_visible = False
filesystem_request_sent = False
filesystem_edit_request_sent = False
live_output_before_finish = False
final_output_after_finish = False
final_seen_before_live = False
filesystem_draft_before_finish = False
filesystem_second_draft_before_finish = False
filesystem_final_after_draft = False
filesystem_final_seen_before_draft = False
filesystem_draft_identity_stable = True
filesystem_edit_draft_before_finish = False
filesystem_edit_second_draft_before_finish = False
filesystem_edit_final_after_draft = False
filesystem_edit_final_seen_before_draft = False
filesystem_edit_draft_identity_stable = True
assistant_request_sent = False
assistant_composer_edit_responsive = False
assistant_markdown_focus_responsive = False
assistant_markdown_activation_responsive = False
assistant_resized_narrow = False
assistant_resized_wide = False
assistant_jumped_latest = False
assistant_redetached = False
assistant_prefix_before_finish = False
assistant_final_after_prefix = False
assistant_suffix_before_prefix = False
cancellation_request_sent = False
cancellation_prefix_visible = False
cancellation_responsive = False
reasoning_request_sent = False
reasoning_first_before_second = False
reasoning_second_after_first = False
reasoning_final_after_updates = False
viewport_detached = False
viewport_anchor = None
viewport_stable = True
viewport_stable_during_stream = False
viewport_check_frames = 0
raw_argument_json_visible = False
running_timeout_visible = False
next_screen_probe = 0.0
screen_frames = []
live_marker = b"FRESHLIVEOUTPUT"
final_marker = b"FRESHFINALOUTPUT"
filesystem_path_marker = b"pty-progressive.txt"
filesystem_first_marker = b"PTYFILESYSTEMFIRST"
filesystem_second_marker = b"PTYFILESYSTEMSECOND"
filesystem_edit_marker = b"PTYFILESYSTEMEDITED"
assistant_prefix_marker = b"ASSISTANTPREFIX"
assistant_suffix_marker = b"ASSISTANTSUFFIX"
reasoning_first_marker = b"  REASONINGFIRST "
reasoning_combined_marker = b"  REASONINGFIRSTREASONINGSECOND "
reasoning_final_marker = b"  REASONINGFINAL "

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

    if time.monotonic() >= next_screen_probe:
        screen = screen_text()
        screen_frames.append(screen)
        if (
            not shell_request_sent
            and b"bcode" in screen
            and session_marker in screen
            and b"ready" in screen
        ):
            os.write(
                fd,
                b"tool-shell pwd; echo FRESHLIVEOUTPUT; sleep 4; echo FRESHFINALOUTPUT\r",
            )
            shell_request_sent = True
        if shell_request_sent and not shell_permission_responsive and b"approve once" in screen.lower():
            os.write(fd, b"\r")
            shell_permission_responsive = True
        if shell_request_sent and shell_permission_responsive and not filesystem_request_sent:
            running_shell = b"running tool: shell" in screen.lower()
            final_shell = b"shell run" in screen.lower() and b"exit code" in screen.lower()
            raw_argument_json_visible |= running_shell and (
                b'"command"' in screen or b"arguments" in screen.lower()
            )
            if running_shell and b"timeout 30.0s" in screen.lower():
                running_timeout_visible = True
            if b"pwd; echo FRESHLIVEOUTPUT" in screen:
                shell_command_visible = True
            if " . ❯".encode() in screen or b"bcode-smoke." in screen:
                shell_cwd_visible = True
            output_lines = {
                line.strip()
                for line in screen.splitlines()
                if line.startswith(b"    ")
            }
            live_output = live_marker in output_lines
            final_output = final_marker in output_lines
            if running_shell and live_output and not final_output:
                live_output_before_finish = True
            if final_shell and final_output:
                final_output_after_finish = True
            if final_output and not live_output_before_finish:
                final_seen_before_live = True
            if (
                live_output_before_finish
                and final_output_after_finish
                and b"ready" in screen.lower()
            ):
                os.write(
                    fd,
                    b"tool-write pty-progressive.txt PTYFILESYSTEMFIRST\\nPTYFILESYSTEMSECOND\r",
                )
                filesystem_request_sent = True
        if filesystem_request_sent:
            lower_screen = screen.lower()
            filesystem_draft = (
                b"filesystem write" in lower_screen
                and b"writing file" not in lower_screen
            )
            filesystem_second_draft = (
                b"filesystem write" in lower_screen
                and b"writing file" in lower_screen
                and filesystem_path_marker in screen
            )
            filesystem_final = (
                (b"wrote" in lower_screen and filesystem_path_marker in screen)
                or b"fake tool result: wrote" in lower_screen
            )
            if filesystem_draft and not filesystem_second_draft and not filesystem_final:
                filesystem_draft_before_finish = True
                filesystem_draft_identity_stable &= lower_screen.count(b"filesystem write") == 1
            if filesystem_second_draft and not filesystem_final:
                filesystem_second_draft_before_finish = True
                filesystem_draft_identity_stable &= lower_screen.count(b"filesystem write") == 1
            if filesystem_final and filesystem_draft_before_finish:
                filesystem_final_after_draft = True
            if filesystem_final and not filesystem_draft_before_finish:
                filesystem_final_seen_before_draft = True
            if (
                filesystem_final_after_draft
                and b"fake tool result: wrote" in lower_screen
                and not filesystem_edit_request_sent
                and b"tool-write" not in lower_screen
            ):
                os.write(fd, b"\x15")
                os.write(
                    fd,
                    b"tool-edit pty-progressive.txt PTYFILESYSTEMSECOND PTYFILESYSTEMEDITED\r",
                )
                filesystem_edit_request_sent = True
        if filesystem_edit_request_sent:
            lower_screen = screen.lower()
            filesystem_edit_draft = (
                b"filesystem edit" in lower_screen
                and b"editing file" not in lower_screen
            )
            filesystem_edit_second_draft = (
                b"filesystem edit" in lower_screen
                and b"editing file" in lower_screen
                and filesystem_path_marker in screen
                and filesystem_second_marker in screen
                and filesystem_edit_marker in screen
            )
            filesystem_edit_final = b"applied 1 replacement" in lower_screen
            if (
                filesystem_edit_draft
                and not filesystem_edit_second_draft
                and not filesystem_edit_final
            ):
                filesystem_edit_draft_before_finish = True
                filesystem_edit_draft_identity_stable &= lower_screen.count(b"filesystem edit") == 1
            if filesystem_edit_second_draft and not filesystem_edit_final:
                filesystem_edit_second_draft_before_finish = True
                filesystem_edit_draft_identity_stable &= lower_screen.count(b"filesystem edit") == 1
            if filesystem_edit_final and filesystem_edit_draft_before_finish:
                filesystem_edit_final_after_draft = True
            if filesystem_edit_final and not filesystem_edit_draft_before_finish:
                filesystem_edit_final_seen_before_draft = True
        if (
            filesystem_edit_final_after_draft
            and not viewport_detached
            and not assistant_request_sent
            and b"ready" in lower_screen
            and b"tool-edit" not in lower_screen
        ):
            os.write(fd, b"\x1b[5~")
            viewport_detached = True
            next_screen_probe = time.monotonic()
        if viewport_detached and not assistant_request_sent:
            candidate = next(
                (
                    line.strip()
                    for line in screen.splitlines()
                    if line.strip()
                    and b"ready" not in line.lower()
                    and b"ask bcode" not in line.lower()
                ),
                None,
            )
            if viewport_anchor is None and candidate is not None:
                viewport_anchor = candidate
            elif viewport_anchor is not None and viewport_anchor in screen:
                viewport_check_frames += 1
            viewport_stable &= viewport_anchor is None or viewport_anchor in screen
        if (
            filesystem_edit_final_after_draft
            and not assistant_request_sent
            and viewport_anchor is not None
            and viewport_check_frames >= 2
            and b"ready" in lower_screen
            and b"tool-edit" not in lower_screen
        ):
            os.write(
                fd,
                b"stream-text # ASSISTANTPREFIX report\n\n- first item\n- second item\n\n| Key | Value |\n| --- | --- |\n| A | B |\n\n```rust\nfn main() {}\n```\n\n<details><summary>More</summary>Detail body</details>\n\nFootnote ref[^1].\n\n[^1]: Footnote body.\n\nUnicode \xe6\x9d\xb1\xe4\xba\xac \xf0\x9f\xa7\xaa \xe2\x9c\x93\n\n![image alt](https://example.com/image.png)\n\n```mermaid\ngraph TD; A-->B;\n```\n\n[ASSISTANTSUFFIX](https://example.com)\r",
            )
            assistant_request_sent = True
        if viewport_detached and assistant_request_sent and viewport_anchor is not None:
            viewport_stable &= viewport_anchor in screen
            if viewport_anchor in screen:
                viewport_stable_during_stream = True
        if assistant_request_sent:
            prefix_visible = assistant_prefix_marker in screen
            suffix_visible = assistant_suffix_marker in screen
            if prefix_visible and not suffix_visible and not assistant_composer_edit_responsive:
                os.write(fd, b"composer-remains-responsive")
                assistant_composer_edit_responsive = True
            if prefix_visible and not suffix_visible:
                assistant_prefix_before_finish = True
            if suffix_visible and not prefix_visible and not assistant_prefix_before_finish:
                assistant_suffix_before_prefix = True
            if prefix_visible and suffix_visible and assistant_prefix_before_finish:
                assistant_final_after_prefix = True
            if (
                assistant_final_after_prefix
                and assistant_redetached
                and not cancellation_request_sent
            ):
                os.write(fd, b"\x15")
                os.write(fd, b"stream-text CANCELPREFIXCANCELSUFFIX\r")
                cancellation_request_sent = True
        if assistant_final_after_prefix and not assistant_markdown_focus_responsive:
            os.write(fd, b"\x1b[1;5I")
            assistant_markdown_focus_responsive = True
        elif assistant_markdown_focus_responsive and not assistant_markdown_activation_responsive:
            os.write(fd, b"\x1b\r")
            assistant_markdown_activation_responsive = True
        elif assistant_markdown_activation_responsive and not assistant_resized_narrow:
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 26, 82, 0, 0))
            os.kill(pid, signal.SIGWINCH)
            assistant_resized_narrow = True
        elif assistant_resized_narrow and not assistant_resized_wide:
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
            os.kill(pid, signal.SIGWINCH)
            assistant_resized_wide = True
        elif assistant_resized_wide and not assistant_jumped_latest:
            os.write(fd, b"\x1b[1;5F")
            assistant_jumped_latest = True
        elif assistant_jumped_latest and not assistant_redetached:
            os.write(fd, b"\x1b[5~")
            assistant_redetached = True
        if cancellation_request_sent and not cancellation_responsive:
            cancel_prefix = b"CANCELPREFIX" in screen
            cancel_suffix = b"CANCELSUFFIX" in screen
            if cancel_prefix and not cancel_suffix:
                cancellation_prefix_visible = True
                os.write(fd, b"\x03")
                cancellation_responsive = True
        if (
            cancellation_responsive
            and not reasoning_request_sent
            and b"ready" in screen.lower()
            and b"stream-text" not in screen.lower()
        ):
            os.write(fd, b"\x15")
            os.write(fd, b"stream-reasoning REASONINGFIRSTREASONINGSECOND\r")
            reasoning_request_sent = True
        if reasoning_request_sent:
            first_visible = reasoning_first_marker in screen
            combined_visible = reasoning_combined_marker in screen
            final_visible = reasoning_final_marker in screen
            if first_visible:
                reasoning_first_before_second = True
            if combined_visible or final_visible:
                reasoning_second_after_first = True
            if final_visible:
                reasoning_final_after_updates = True
        next_screen_probe = time.monotonic() + 0.25

    if (
        not exit_requested
        and shell_request_sent
        and shell_permission_responsive
        and filesystem_request_sent
        and filesystem_edit_request_sent
        and live_output_before_finish
        and final_output_after_finish
        and filesystem_draft_before_finish
        and filesystem_second_draft_before_finish
        and filesystem_final_after_draft
        and filesystem_edit_draft_before_finish
        and filesystem_edit_second_draft_before_finish
        and filesystem_edit_final_after_draft
        and assistant_request_sent
        and assistant_composer_edit_responsive
        and assistant_prefix_before_finish
        and assistant_final_after_prefix
        and assistant_markdown_focus_responsive
        and assistant_markdown_activation_responsive
        and assistant_resized_narrow
        and assistant_resized_wide
        and assistant_jumped_latest
        and assistant_redetached
        and cancellation_request_sent
        and cancellation_prefix_visible
        and cancellation_responsive
        and reasoning_request_sent
        and reasoning_first_before_second
        and reasoning_second_after_first
        and reasoning_final_after_updates
        and viewport_detached
        and viewport_anchor is not None
        and viewport_stable_during_stream
    ):
        try:
            os.write(fd, b"\x04")
        except OSError:
            pass
        exit_requested = True
        exit_deadline = time.monotonic() + 10

    waited_pid, status = os.waitpid(pid, os.WNOHANG)
    if waited_pid:
        exit_status = status
        break
    if exit_deadline is not None and time.monotonic() >= exit_deadline:
        break

if exit_status is None:
    try:
        os.write(fd, b"\x1b")
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
with open(capture_path + ".frames", "wb") as frames_file:
    for index, screen in enumerate(screen_frames):
        frames_file.write(f"\n--- frame {index} ---\n".encode())
        frames_file.write(screen)

metadata_markers = (
    b"runtime_work_started",
    b"runtime_work_finished",
    b"model_usage",
    b"request_context_observed",
    b"trace_event",
    b"input_tokens",
    b"output_tokens",
    b"reasoning_tokens",
)
metadata_excluded = all(
    all(marker not in screen.lower() for marker in metadata_markers)
    for screen in screen_frames
)

checks = {
    "alternate-screen entry": b"\x1b[?1049h" in capture,
    "alternate-screen restoration": b"\x1b[?1049l" in capture,
    "bracketed-paste entry": b"\x1b[?2004h" in capture,
    "bracketed-paste restoration": b"\x1b[?2004l" in capture,
    "rendered Bcode frame": b"bcode" in capture,
    "rendered session identity": session_marker in capture,
    "rendered provider status": b"provider" in capture,
    "shell request sent": shell_request_sent,
    "permission dialog responsive during workflow": shell_permission_responsive,
    "live output visible before command completion": live_output_before_finish,
    "shell command remains visible while running": shell_command_visible,
    "shell cwd remains visible while running": shell_cwd_visible,
    "running shell shows effective timeout": running_timeout_visible,
    "raw argument JSON never visible": not raw_argument_json_visible,
    "final output did not precede live output": not final_seen_before_live,
    "final output visible after command completion": final_output_after_finish,
    "filesystem request sent": filesystem_request_sent,
    "filesystem first draft visible before completion": filesystem_draft_before_finish,
    "filesystem second draft visible before completion": filesystem_second_draft_before_finish,
    "filesystem final did not precede draft": not filesystem_final_seen_before_draft,
    "filesystem final visible after draft": filesystem_final_after_draft,
    "filesystem draft has one invocation presentation": filesystem_draft_identity_stable,
    "filesystem edit request sent": filesystem_edit_request_sent,
    "filesystem edit first draft visible before completion": filesystem_edit_draft_before_finish,
    "filesystem edit second draft visible before completion": filesystem_edit_second_draft_before_finish,
    "filesystem edit final did not precede draft": not filesystem_edit_final_seen_before_draft,
    "filesystem edit final visible after draft": filesystem_edit_final_after_draft,
    "filesystem edit draft has one invocation presentation": filesystem_edit_draft_identity_stable,
    "assistant streaming request sent": assistant_request_sent,
    "composer accepts edits during assistant streaming": assistant_composer_edit_responsive,
    "assistant prefix visible before completion": assistant_prefix_before_finish,
    "assistant suffix did not precede prefix": not assistant_suffix_before_prefix,
    "assistant final preserves prefix and suffix": assistant_final_after_prefix,
    "Markdown focus responds after streaming": assistant_markdown_focus_responsive,
    "Markdown activation responds after streaming": assistant_markdown_activation_responsive,
    "narrow resize handled": assistant_resized_narrow,
    "wide resize handled": assistant_resized_wide,
    "jump to latest handled": assistant_jumped_latest,
    "viewport detaches again after jump": assistant_redetached,
    "cancellation stream request sent": cancellation_request_sent,
    "cancellation prefix visible before completion": cancellation_prefix_visible,
    "cancellation remains responsive during streaming": cancellation_responsive,
    "reasoning streaming request sent": reasoning_request_sent,
    "reasoning first update visible before second": reasoning_first_before_second,
    "reasoning second update preserves first": reasoning_second_after_first,
    "reasoning final follows ordered updates": reasoning_final_after_updates,
    "viewport detached before operational updates": viewport_detached and viewport_anchor is not None,
    "detached viewport anchor remains visible": viewport_stable_during_stream,
    "usage and runtime metadata excluded from transcript": metadata_excluded,
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

    "${root}/target/debug/bcode" server stop >/dev/null 2>&1 &
    stop_pid=$!
    for _ in {1..100}; do
        if ! kill -0 "${stop_pid}" 2>/dev/null; then
            wait "${stop_pid}" || true
            break
        fi
        sleep 0.1
    done
wait "${server_pid}" || true
server_pid=""

echo "smoke-tui-pty: PASS"
