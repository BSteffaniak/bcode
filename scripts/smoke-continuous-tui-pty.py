#!/usr/bin/env python3
"""Offline PTY acceptance, without builds or the caller's configuration/state.

Usage (after the coordinator grants runtime capacity):
  bash scripts/smoke-continuous-tui-pty.sh --binary /absolute/bcode \
    --grid-probe /absolute/bcode_terminal_grid_probe \
    --fake-library /absolute/libbcode_fake_provider_plugin.dylib \
    --case quick-advanced-cancel

Built-in cases assert only the named journey, not the entire product acceptance.
Custom --plan JSON contract:
  {"name":"review-home-workspace-chat", "model":true, "args":["review"],
   "steps":[{"expect":["Review"], "send":"\\r"},
            {"expect":["Workspace"], "send":"..."},
            {"expect":["Ask Bcode"], "send":"\\u0004"}], "exit_code":0}
Expectations are case-insensitive substrings of the *current rendered grid*, not
source assertions or historical terminal output. A step may have forbid:[...],
resize:[columns,rows], timeout:seconds, hold:seconds, and send:text. Use hold to
keep a delayed initializer visible while repeatedly checking raw mode/containment.
Default hold is 0.5 seconds so a transition does not consume the next screen's key.
--cold-start lets the product start its own isolated daemon; cleanup uses its
public server stop against the explicit test artifact and isolated state only.
blims-service-timeout creates company/world with public CLI, suspends only an
unreaped direct daemon child, checks visible unknown-outcome recovery and resumes
that child before cleanup. It never retries ambiguous changes.
Every step must expect
visible content; reject:[...] fails immediately on forbidden side effects, whereas
forbid:[...] waits for stale transition content to disappear. Completion cases
exercise plugin-owned path discovery and keep parents open after accept/Escape;
they do not claim submission or workflow execution acceptance.
Custom plans use fake-only fixtures. The dedicated xai-model-chat case enables
bundled compatible local recommendation with a fake XAI_API_KEY, disables remote
catalogs and sends no prompt. Darwin sandbox-exec denies IP networking for both
client and daemon (Unix IPC remains allowed); other platforms fail closed for this
case. This is not auth verification/provider response
coverage. Successful enrollment still needs a separate safe fixture.

Captures, final grid, daemon log and JSON evidence stay in a private temporary
folder (reported on completion/failure). Fake/review cases enable only fake-provider
and local default-agent/code-review plugins. xAI case uses bundled compatible only
for local recommendation, with a fake key and zero chat submissions. No session is fabricated
for setup. Cleanup signals only direct children we started and have not reaped.
--self-test tests harness mechanics only, never application acceptance.
"""

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

MAX_CAPTURE = 8 * 1024 * 1024
MODE = re.compile(rb"\x1b\[\?([0-9;]+)([hl])")
ALT_MODES = {47, 1047, 1049}
MARKER = b"BCODE_CONTINUOUS_PTY_SHELL_SENTINEL"
def completion_steps(parent, exit_steps):
    """Keep the focused parent open through accept, Escape, and ordinary edits."""
    guard = ["COMPLETION_PARENT_SUBMITTED"]
    return [
        {"expect": [parent], "hold": 2, "send": "@completion-t"},
        {"expect": ["Suggestions", "completion-target.txt"], "send": "\r"},
        {"expect": [parent, "@completion-target.txt"], "forbid": ["Suggestions"],
         "reject": guard, "hold": 1, "send": " ordinary"},
        {"expect": [parent, "@completion-target.txt ordinary"], "reject": guard,
         "send": "\x15@completion-t"},
        {"expect": ["Suggestions", "completion-target.txt"], "reject": guard,
         "send": "\x1b"},
        {"expect": [parent, "@completion-t"],
         "forbid": ["Suggestions", "completion-target.txt"], "reject": guard,
         "send": " ordinary"},
        {"expect": [parent, "@completion-t ordinary"], "reject": guard,
         "send": "\x1b"},
    ] + exit_steps


CASES = {
    "completion-chat": {"fixture": "completion", "model": True,
        "steps": completion_steps("Message", [
            {"expect": ["Message"], "send": "\x04"},
            {"expect": ["input cleared", "Message"], "send": "\x04"}])},
    "completion-loop-goal": {"fixture": "completion", "model": True,
        "steps": [{"expect": ["Message"], "send": "/goal\r"},
                  {"expect": ["Message", "/goal"], "send": "\r"}] +
        completion_steps("Goal (required)", [
            {"expect": ["Ask Bcode"], "send": "\x04"}])},
    "completion-question": {"fixture": "completion", "model": True,
        "steps": [{"expect": ["Message", "ready"], "hold": 2, "send": 'tool-call question {"questions":[{"header":"Completion","question":"COMPLETION_QUESTION","options":[],"custom":true,"control":"radio","selection_mode":"single","required":true}]}\r'}] +
        completion_steps("COMPLETION_QUESTION", [
            {"expect": ["Ask Bcode"], "send": "\x04"}])},
    "completion-review": {"fixture": "completion-review", "model": True, "args": ["review"],
        "steps": [{"expect": ["Bcode Reviews"], "send": "s"},
                  {"expect": ["Staged", "CONTINUOUS_REVIEW_FILE"], "send": "ja"}] +
        completion_steps("ask Bcode", [
            {"expect": ["Staged", "CONTINUOUS_REVIEW_FILE"], "send": "\x04"}])},

    "blims-service-timeout": {"fixture": "blims", "model": True,
        "args": ["blims", "enter"], "steps": [
        {"expect": ["Pixel office"], "daemon_signal": "suspend", "send": "r"},
        {"expect": ["Outcome unknown", "changes paused"], "hold": 0.5,
         "daemon_signal": "resume", "send": "q", "timeout": 60}]},
    "xai-model-chat": {"fixture": "xai", "model": False, "args": [], "steps": [
        {"expect": ["Welcome to Bcode", "xAI", "grok"], "send": "\r"},
        {"expect": ["Ask Bcode"], "hold": 1, "send": "\x04"}]},
    "review-home-cancel": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Bcode Reviews"], "send": "\x04"}]},
    "review-invalid-repository": {"fixture": "fake", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Bcode Reviews"], "send": "v"},
        {"expect": ["failed to create workspace"], "send": "\x04"}]},
    "review-home-new-workspace": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Bcode Reviews"], "send": "n"},
        {"expect": ["new review title"], "send": "CONTINUOUS_PTY_REVIEW\r"},
        {"expect": ["CONTINUOUS_PTY_REVIEW"], "forbid": ["Bcode Reviews", "new review title"], "send": "\x04"}]},
    "review-linked-navigation": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Bcode Reviews"], "send": "s"},
        {"expect": ["Staged", "CONTINUOUS_REVIEW_FILE"], "forbid": ["Bcode Reviews"], "send": "ja"},
        {"expect": ["CONTINUOUS_REVIEW_FILE", "ask Bcode"], "forbid": ["select a diff line or range"],
         "send": "CONTINUOUS_REVIEW_QUESTION\r"},
        {"expect": ["CONTINUOUS_REVIEW_QUESTION", "session linked:"], "send": "o", "timeout": 60},
        {"expect": ["Ask Bcode", "CONTINUOUS_REVIEW_QUESTION"], "hold": 0.5, "send": "\x04"},
        {"expect": ["bcode review", "Staged"], "send": "\x04"}]},
    "review-home-workspace-chat": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Bcode Reviews"], "send": "s"},
        {"expect": ["Staged", "CONTINUOUS_REVIEW_FILE"], "forbid": ["Bcode Reviews"], "send": "ja"},
        {"expect": ["CONTINUOUS_REVIEW_FILE", "ask Bcode"], "forbid": ["select a diff line or range"],
         "send": "CONTINUOUS_REVIEW_QUESTION\r"},
        {"expect": ["CONTINUOUS_REVIEW_QUESTION", "answered"], "send": "o", "timeout": 60},
        {"expect": ["Ask Bcode", "CONTINUOUS_REVIEW_QUESTION"], "hold": 0.5, "send": "\x04"},
        {"expect": ["bcode review", "Staged"], "send": "\x04"}]},
    "quick-cancel": {"model": False, "args": [], "steps": [
        {"expect": ["Welcome to Bcode", "No usable account"], "send": "\x1b"}]},
    "quick-advanced-cancel": {"model": False, "args": [], "steps": [
        {"expect": ["No usable account"], "send": "\x1bOQ"},
        {"expect": ["Set up your workspace"], "send": "\x1b"}]},
    "advanced-cancel": {"model": True, "args": ["--onboard"], "steps": [
        {"expect": ["Set up your workspace"], "send": "\x1b"}]},
    "advanced-enrollment-cancel": {"model": True, "args": ["--onboard"], "steps": [
        {"expect": ["Set up your workspace"], "send": "p"},
        {"expect": ["Choose a provider", "Fake provider"], "send": "\x1b"},
        {"expect": ["Set up your workspace"], "send": "\x1b"}]},
    "advanced-chat": {"model": True, "args": ["--onboard"], "steps": [
        {"expect": ["Set up your workspace"], "send": "l"},
        {"expect": ["Ask Bcode"], "send": "CONTINUOUSPTYECHO\r"},
        {"expect": ["CONTINUOUSPTYECHO", "fake"], "send": "\x04"}]},
}


def digest(path):
    with open(path, "rb") as source:
        checksum = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(block)
        return checksum.hexdigest()


def stop(child):
    """Only signal an unreaped direct child (no pid discovery or process sweeps)."""
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=30)


class Interval:
    def __init__(self):
        self.enters = 0
        self.leaves = 0
        self.active = False
        self.end = 0

    def scan(self, capture):
        # Revisit an unfinished escape sequence, never recount a complete one.
        for match in MODE.finditer(capture, self.end):
            self.end = match.end()
            if not ALT_MODES.intersection(map(int, match[1].split(b";"))):
                continue
            if match[2] == b"h":
                self.enters += 1
                self.active = True
                if self.enters != 1:
                    raise AssertionError("alternate screen re-entered during journey")
            else:
                self.leaves += 1
                if not self.active:
                    raise AssertionError("alternate screen left without entry")
                self.active = False

    def finished(self):
        if (self.enters, self.leaves, self.active) != (1, 1, False):
            raise AssertionError(f"expected one screen interval, got {self.enters}/{self.leaves}")


def pump(master, capture, interval, timeout=0.05):
    readable, _, _ = select.select([master], [], [], timeout)
    if readable:
        try:
            chunk = os.read(master, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            chunk = b""
        capture.extend(chunk)
        if len(capture) > MAX_CAPTURE:
            raise AssertionError("bounded PTY capture exceeded 8 MiB")
        interval.scan(capture)


def raw(slave):
    return not termios.tcgetattr(slave)[3] & (termios.ICANON | termios.ECHO)


def run_pty(command, env, work, steps, probe, timeout, exit_code=0, daemon=None,
            failure_status_command=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 80, 120, 0, 0))
    initial = termios.tcgetattr(slave)
    # Model a pre-existing shell prompt in the primary buffer. A real alternate
    # screen must hide it for every screen, including intermediate transitions.
    os.write(slave, MARKER + b"\r\n")
    child = None
    capture = bytearray()
    interval = Interval()
    evidence = []
    deadline = time.monotonic() + timeout
    capture_path = work / "terminal.capture"
    last_grid = ""
    failure = None

    def screen():
        nonlocal last_grid
        capture_path.write_bytes(capture)
        if probe is None:  # Self-test fixture only; never production rendering.
            last_grid = bytes(capture).decode("utf-8", "replace")
        else:
            rows, cols, _, _ = struct.unpack(
                "HHHH", fcntl.ioctl(slave, termios.TIOCGWINSZ, b"\0" * 8))
            result = subprocess.run([probe, str(capture_path), str(cols), str(rows)],
                                    capture_output=True, check=True, timeout=15, env=env)
            if len(result.stdout) > MAX_CAPTURE:
                raise AssertionError("grid probe exceeded output bound")
            last_grid = result.stdout.decode("utf-8", "replace")
            if MARKER.decode() in last_grid:
                raise AssertionError("primary-buffer shell sentinel exposed during journey")
        (work / "last-grid.txt").write_text(last_grid)
        return last_grid.lower()

    try:
        # Establish a controlling tty, like an interactive foreground shell child.
        # This single-threaded harness is the only place we use preexec_fn.
        def child_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        child = subprocess.Popen(command, cwd=work / "repo" if (work / "repo").is_dir() else work,
                                 env=env, stdin=slave,
                                 stdout=slave, stderr=slave, preexec_fn=child_terminal)
        for index, step in enumerate(steps):
            end = min(deadline, time.monotonic() + step.get("timeout", 45))
            matched = False
            hold_until = None
            while time.monotonic() < end:
                pump(master, capture, interval)
                if interval.leaves:
                    raise AssertionError(f"screen left before step {index + 1} completed")
                if interval.active:
                    if not raw(slave):
                        raise AssertionError("raw input released before final exit")
                    grid = screen()
                    rejected = [value for value in step.get("reject", []) if value.lower() in grid]
                    if rejected:
                        raise AssertionError(f"step {index + 1} forbidden side effect: {rejected}")
                    forbidden = any(value.lower() in grid for value in step.get("forbid", []))
                    # Prior-screen text may legitimately remain until the next paint.
                    # Do not treat it as a match or advance the journey prematurely.
                    if not forbidden and all(value.lower() in grid for value in step["expect"]):
                        if hold_until is None:
                            hold_until = time.monotonic() + step.get("hold", 0.5)
                        if time.monotonic() >= hold_until:
                            matched = True
                            break
                    else:
                        hold_until = None
                if child.poll() is not None:
                    raise AssertionError(f"application exited before step {index + 1}")
            if not matched:
                raise AssertionError(f"timeout waiting for rendered step {index + 1}: {step['expect']}")
            (work / f"step-{index + 1:02d}-grid.txt").write_text(last_grid)
            evidence.append({"step": index + 1, "expect": step["expect"], "raw": True,
                             "reject": step.get("reject", []),
                             "daemon_signal": step.get("daemon_signal"),
                             "alternate_screen_enters": interval.enters})
            if step.get("daemon_signal"):
                if daemon is None or daemon.poll() is not None:
                    raise AssertionError("daemon fault injection requires unreaped harness-created child")
                daemon.send_signal(signal.SIGSTOP if step["daemon_signal"] == "suspend" else signal.SIGCONT)
            if "resize" in step:
                cols, rows = step["resize"]
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                # Child is still our unreaped direct process.
                if child.poll() is None:
                    child.send_signal(signal.SIGWINCH)
            if step.get("send"):
                os.write(master, step["send"].encode())
        end = min(deadline, time.monotonic() + 10)
        while child.poll() is None and time.monotonic() < end:
            pump(master, capture, interval)
        if child.poll() is None:
            raise AssertionError("application failed to exit; cleanup is not acceptance")
        # Drain after child termination, before checking leave sequence/restoration.
        while select.select([master], [], [], 0.05)[0]:
            before = len(capture)
            pump(master, capture, interval, 0)
            if len(capture) == before:
                break
        interval.finished()
        if child.returncode != exit_code:
            raise AssertionError(f"exit status {child.returncode}, expected {exit_code}")
        if termios.tcgetattr(master) != initial:
            raise AssertionError("application did not restore exact original tty attributes")
        # After the controlling session exits Darwin revokes slave ioctl access;
        # tty attributes remain observable through the master. Shell output can
        # be tested while the child is alive, but not by writing a revoked slave.
    except (AssertionError, OSError, subprocess.SubprocessError) as error:
        failure = str(error)
        # Read public bounded catalog status while the failing foreground and
        # its exact isolated daemon are still alive. Never retry the model turn.
        capture_path.write_bytes(capture)
        if failure_status_command is not None:
            try:
                result = subprocess.run(failure_status_command, env=env, cwd=work,
                                        capture_output=True, timeout=20)
                (work / "failure-session-status.txt").write_bytes(
                    result.stdout + result.stderr)
                (work / "failure-session-status-exit.txt").write_text(str(result.returncode))
            except subprocess.SubprocessError as diagnostic_error:
                (work / "failure-session-status.txt").write_text(str(diagnostic_error))
        raise
    finally:
        # Check restoration before forced cleanup above; never conceal a defect by
        # restoring termios ourselves on the private PTY.
        cleanup_error = None
        try:
            stop(child)
        except subprocess.SubprocessError as error:
            cleanup_error = str(error)
        capture_path.write_bytes(capture)
        (work / "evidence.json").write_text(json.dumps({
            "cleanup_error": cleanup_error,
            "command": command, "steps": evidence, "enters": interval.enters,
            "leaves": interval.leaves, "failure": failure,
            "step_timeout_seconds": timeout,
            "tty_restored": termios.tcgetattr(master) == initial,
            "exit_code": child.returncode if child else None}, indent=2))
        if cleanup_error and failure is None:
            raise AssertionError(f"owned child cleanup failed: {cleanup_error}")
        os.close(master)
        os.close(slave)


def isolated_fixture(work, library, model, fixture="fake"):
    home = work / "home"
    config_dir = home / ".config"
    state = work / "state"
    tmp = work / "tmp"
    for directory in (home, config_dir, state, tmp):
        directory.mkdir(parents=True, exist_ok=True)
    if fixture != "xai":
        manifest_dir = config_dir / "bcode/plugins/fake-provider"
        manifest_dir.mkdir(parents=True)
        template = Path(__file__).resolve().parent.parent / "plugins/fake-provider-plugin/bcode-plugin.toml"
        manifest, count = re.subn(r'(?m)^library\s*=\s*"[^"]+"$',
                                  "library = " + json.dumps(library), template.read_text())
        if count != 1:
            raise AssertionError("fake manifest must have one runtime library")
        (manifest_dir / "bcode-plugin.toml").write_text(manifest)
    config = work / "bcode.toml"
    text = '''[plugins]
default = "none"
enabled = ["bcode.fake-provider", "bcode.default-agents", "bcode.code_review"]
disabled = ["bcode.openai-compatible", "bcode.bedrock"]
[model.prompt_cache]
mode = "off"
[daemon]
idle_shutdown = false
'''
    if fixture in ("completion", "completion-review"):
        text = text.replace('"bcode.code_review"',
                            '"bcode.code_review", "bcode.file-references", "bcode.filesystem", '
                            '"bcode.question", "bcode.loop", "bcode.workflow"')
        text += '[tui.interactions]\nplacement = "pinned"\n'
        # Debug nested IPC needs headroom; keep the production search budget bounded.
        text += '[file_references]\ntimeout_ms = 1000\n'
        (work / "completion-target.txt").write_text("offline path completion fixture\n")
    if fixture == "blims":
        text = text.replace('"bcode.code_review"', '"bcode.code_review", "bcode.blims"')
        text += '[client]\nrequest_timeout_secs = 2\n'
    if fixture == "xai":
        text = text.replace('"bcode.fake-provider", ', '"bcode.openai-compatible", ')
        text = text.replace('disabled = ["bcode.openai-compatible", "bcode.bedrock"]',
                            'disabled = ["bcode.bedrock"]')
    if model:
        text += '''[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
'''
    if fixture in ("completion", "completion-review"):
        text += '''
[model.profiles.pty-completion]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
'''
    if fixture in ("review", "completion", "completion-review"):
        # A submitted parent emits a visible sentinel in completion fixtures.
        # Initial question tool requests still use the normal fake tool directive.
        answer = "CONTINUOUS_REVIEW_ANSWER" if fixture == "review" else "COMPLETION_PARENT_SUBMITTED"
        script = {"steps": [
            {"event": {"type": "text_delta", "text": answer}},
            {"event": {"type": "turn_finished", "stop_reason": "end_turn"}}]}
        if fixture == "review":
            text += "[model.settings]\nfake_event_script = " + json.dumps(json.dumps(script)) + "\n"
        else:
            scripts = {"@completion": script}
            text += "[model.profiles.pty-completion.settings]\nfake_prompt_tool_directives = \"true\"\nfake_event_scripts_by_prompt_marker = " + json.dumps(json.dumps(scripts)) + "\n"
    config.write_text(text)
    # Allowlist, not an inherited environment with an incomplete secret blacklist.
    env = {"HOME": str(home), "XDG_CONFIG_HOME": str(config_dir),
           "XDG_STATE_HOME": str(home / ".local/state"), "XDG_CACHE_HOME": str(home / ".cache"),
           "TMPDIR": str(tmp), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TERM": "xterm-256color",
           "LANG": "en_US.UTF-8", "BCODE_CONFIG": str(config), "BCODE_STATE_DIR": str(state),
           "RUST_MIN_STACK": "16777216", "BCODE_DISABLE_REMOTE_MODEL_CATALOG": "1"}
    if fixture == "xai":
        env["XAI_API_KEY"] = "bcode-pty-fake-not-a-real-credential"
        env.update({"HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
                    "ALL_PROXY": "http://127.0.0.1:1", "NO_PROXY": "localhost,127.0.0.1"})
    if fixture in ("review", "completion", "completion-review"):
        # Keep search workspace separate from harness state/config and build artifacts.
        git_env = {**env, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
        repo = work / "repo"
        repo.mkdir()
        subprocess.run(["/usr/bin/git", "init", "--quiet", str(repo)], env=git_env,
                       check=True, timeout=5, capture_output=True)
        (repo / "continuous-review-fixture.txt").write_text("CONTINUOUS_REVIEW_FILE\n")
        if fixture in ("completion", "completion-review"):
            (repo / "completion-target.txt").write_text("offline path completion fixture\n")
        subprocess.run(["/usr/bin/git", "-C", str(repo), "add", "--", "continuous-review-fixture.txt"],
                       env=git_env, check=True, timeout=5, capture_output=True)
    return env, config, state


def plan_checked(plan):
    if not isinstance(plan, dict) or set(plan) - {"name", "fixture", "model", "args", "steps", "exit_code"}:
        raise ValueError("unknown plan fields")
    if plan.get("fixture", "fake") not in ("fake", "review", "xai", "blims", "completion", "completion-review"):
        raise ValueError("unsupported fixture")
    if plan.get("fixture") == "xai" and plan != CASES["xai-model-chat"]:
        raise ValueError("xAI fixture restricted to fixed no-prompt model-choice journey")
    args = plan.get("args", [])
    if args not in ([], ["--onboard"], ["review"], ["blims", "enter"]):
        raise ValueError("plan args must select startup, --onboard, review, or blims enter; no config/state overrides")
    steps = plan.get("steps", [])
    if not steps or len(steps) > 32:
        raise ValueError("plan requires 1..32 steps")
    for step in steps:
        if not isinstance(step, dict) or set(step) - {"expect", "forbid", "reject", "send", "resize", "timeout", "hold", "daemon_signal"}:
            raise ValueError("unknown step fields")
        for field in ("expect", "forbid", "reject"):
            values = step.get(field, [])
            if not isinstance(values, list) or any(not isinstance(v, str) or not v or len(v) > 256 for v in values):
                raise ValueError("expect/forbid must contain bounded nonempty strings")
        if not step.get("expect") or len(step["expect"]) > 16:
            raise ValueError("each step requires 1..16 visible expectations")
        if not isinstance(step.get("send", ""), str) or len(step.get("send", "")) > 4096:
            raise ValueError("send must be bounded text")
        if not 0 < step.get("timeout", 45) <= 60:
            raise ValueError("step timeout must be in (0,60]")
        if not 0 <= step.get("hold", 0) < step.get("timeout", 45):
            raise ValueError("hold must be nonnegative and shorter than step timeout")
        if step.get("daemon_signal") not in (None, "suspend", "resume"):
            raise ValueError("unsupported daemon signal")
        if "resize" in step:
            size = step["resize"]
            if not isinstance(size, list) or len(size) != 2 or any(not isinstance(v, int) or not 1 <= v <= 500 for v in size):
                raise ValueError("resize requires [columns,rows] in 1..500")
    if not isinstance(plan.get("model", False), bool) or not isinstance(plan.get("exit_code", 0), int):
        raise ValueError("model must be boolean; exit_code must be integer")
    return plan


def self_test():
    interval = Interval()
    interval.scan(b"\x1b[?1049")
    interval.scan(b"\x1b[?1049h")
    interval.scan(b"\x1b[?1049h\x1b[?1049l")
    interval.finished()
    try:
        interval.scan(b"\x1b[?1049h\x1b[?1049l\x1b[?1049h")
    except AssertionError:
        pass
    else:
        raise AssertionError("self-test failed to reject terminal reentry")
    for plan in CASES.values():
        plan_checked(plan)
    work = Path(tempfile.mkdtemp(prefix="bcode-continuous-pty-selftest-"))
    fixture = (
        "import os,termios,tty; old=termios.tcgetattr(0); tty.setraw(0); "
        "os.write(1,b'\\x1b[?1049hHARNESS_READY'); os.read(0,1); "
        "os.write(1,b'\\x1b[?1049l'); termios.tcsetattr(0,termios.TCSANOW,old)"
    )
    run_pty([sys.executable, "-c", fixture], os.environ.copy(), work,
            [{"expect": ["HARNESS_READY"], "hold": 0.2, "send": "q"}], None, 5)
    # Prove failures are not hidden by forced cleanup or historical captures.
    failures = {
        "reentry": (fixture.replace("os.read(0,1);", "os.write(1,b'\x1b[?1049l\x1b[?1049h'); os.read(0,1);"),
                    "alternate screen re-entered"),
        "raw-release": (fixture.replace("os.read(0,1);", "termios.tcsetattr(0,termios.TCSANOW,old); os.read(0,1);"),
                        "raw input released"),
        "unrestored": (fixture.replace("termios.tcsetattr(0,termios.TCSANOW,old)", "None"),
                       "did not restore exact original tty"),
        "missing-screen": (fixture.replace("HARNESS_READY", "NOT_READY"),
                           "timeout waiting"),
    }
    for name, (source, expected) in failures.items():
        case_work = work / name
        case_work.mkdir()
        try:
            run_pty([sys.executable, "-c", source], os.environ.copy(), case_work,
                    [{"expect": ["HARNESS_READY"], "hold": 0, "send": "q", "timeout": 0.5}], None, 2)
        except AssertionError as error:
            if expected not in str(error):
                raise AssertionError(f"wrong {name} failure: {error}") from error
        else:
            raise AssertionError(f"self-test failed to reject {name}")
    reject_work = work / "rejected-side-effect"
    reject_work.mkdir()
    try:
        run_pty([sys.executable, "-c", fixture], os.environ.copy(), reject_work,
                [{"expect": ["HARNESS_READY"], "reject": ["HARNESS_READY"],
                  "hold": 0, "send": "q"}], None, 2)
    except AssertionError as error:
        if "forbidden side effect" not in str(error):
            raise AssertionError(f"wrong reject failure: {error}") from error
    else:
        raise AssertionError("self-test failed to reject a forbidden side effect")
    error_work = work / "initializer-error"
    error_work.mkdir()
    run_pty([sys.executable, "-c", fixture + "; raise SystemExit(1)"],
            os.environ.copy(), error_work,
            [{"expect": ["HARNESS_READY"], "send": "q"}], None, 5, exit_code=1)
    print(f"harness self-test PASS (not product acceptance); artifacts: {work}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary")
    parser.add_argument("--grid-probe")
    parser.add_argument("--fake-library")
    parser.add_argument("--binary-sha256", help="Coordinator-admitted executable digest")
    parser.add_argument("--grid-probe-sha256", help="Coordinator-admitted grid probe digest")
    parser.add_argument("--fake-library-sha256", help="Coordinator-admitted fake plugin digest")
    cases = parser.add_mutually_exclusive_group()
    cases.add_argument("--case", choices=sorted(CASES), default=None)
    cases.add_argument("--plan", type=Path)
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--cold-start", action="store_true", help="Let the TUI own isolated daemon startup")
    parser.add_argument("--self-test", action="store_true")
    options = parser.parse_args()
    if options.self_test:
        self_test()
        return
    if not 0 < options.timeout <= 300:
        parser.error("timeout must be in (0,300]")
    if options.plan and options.plan.stat().st_size > 65536:
        parser.error("plan exceeds 64 KiB")
    plan = plan_checked(json.loads(options.plan.read_text()) if options.plan else CASES[options.case or "quick-cancel"])
    for field in ("binary", "grid_probe") + (() if plan.get("fixture") == "xai" else ("fake_library",)):
        value = getattr(options, field)
        if not value or not Path(value).is_absolute() or not Path(value).is_file():
            parser.error(f"--{field.replace('_', '-')} requires an explicit absolute built artifact")
    for field in ("binary", "grid_probe") + (() if plan.get("fixture") == "xai" else ("fake_library",)):
        expected = getattr(options, field + "_sha256")
        actual = digest(getattr(options, field))
        if expected and actual != expected.lower():
            parser.error(f"{field} SHA256 differs from coordinator admission")
    if not os.access(options.binary, os.X_OK) or not os.access(options.grid_probe, os.X_OK):
        parser.error("binary and grid probe must be executable")
    # Enforce no IP contact, not just a fake credential or proxy preference. IPC
    # Unix sockets remain allowed. Never silently weaken this on another platform.
    sandbox = []
    if sys.platform == "darwin" and Path("/usr/bin/sandbox-exec").is_file():
        sandbox = ["/usr/bin/sandbox-exec", "-p",
                   '(version 1)(allow default)(deny network* (remote ip "*:*") (local ip "*:*") )']
        subprocess.run(sandbox + ["/usr/bin/true"], check=True, timeout=5,
                       capture_output=True)
    elif plan.get("fixture") == "xai":
        parser.error("xAI no-contact case requires Darwin sandbox-exec IP confinement")
    work = Path(tempfile.mkdtemp(prefix="bcode-continuous-pty-"))
    print(f"isolated artifacts: {work}", flush=True)
    env, config, state = isolated_fixture(work, options.fake_library, plan.get("model", False),
                                          plan.get("fixture", "fake"))
    base = [options.binary, "--config-only", str(config), "--state-root", str(state)]
    if plan.get("fixture") in ("completion", "completion-review"):
        base += ["--profile", "pty-completion"]
    if plan.get("fixture") != "xai":
        base.append("--no-credential-discovery")
    identity = subprocess.run(sandbox + base + ["artifact-id"], env=env, cwd=work,
                              check=True, capture_output=True, timeout=60).stdout.decode().strip()
    (work / "artifacts.json").write_text(json.dumps({
        "artifact_id": identity, "binary_sha256": digest(options.binary),
        "grid_probe_sha256": digest(options.grid_probe),
        "fake_library_sha256": digest(options.fake_library) if plan.get("fixture") != "xai" else None,
        "cold_start": options.cold_start,
        "case": plan.get("name", options.case or "custom"),
        "network_confinement": "Darwin sandbox denies IP network" if sandbox else "fake-only provider"}, indent=2))
    daemon = None
    try:
        with (work / "daemon.log").open("wb") as log:
            if not options.cold_start:
                daemon = subprocess.Popen(sandbox + base + ["server", "run"],
                                          cwd=work / "repo" if (work / "repo").is_dir() else work, env=env,
                                          stdin=subprocess.DEVNULL, stdout=log, stderr=log)
                end = time.monotonic() + min(60, options.timeout)
                while time.monotonic() < end:
                    if daemon.poll() is not None:
                        raise AssertionError("isolated daemon exited before readiness; see daemon.log")
                    if (work / "daemon.log").stat().st_size > MAX_CAPTURE:
                        raise AssertionError("daemon startup log exceeded bound")
                    if b"server ready; accepting clients" in (work / "daemon.log").read_bytes():
                        break
                    time.sleep(0.1)
                else:
                    raise AssertionError("isolated daemon readiness timeout")
            if plan.get("fixture") == "blims":
                if options.cold_start:
                    raise ValueError("Blims fault fixture requires harness-owned daemon")
                result = subprocess.run(sandbox + base + ["blims", "create", "--json"],
                                        env=env, cwd=work, capture_output=True, timeout=60)
                (work / "blims-create.json").write_bytes(result.stdout)
                if result.returncode:
                    raise AssertionError("isolated Blims company.create failed")
                result = subprocess.run(sandbox + base + ["blims", "select-world", "cozy-startup-loft", "--json"],
                                        env=env, cwd=work, capture_output=True, timeout=60)
                (work / "blims-world.json").write_bytes(result.stdout)
                if result.returncode:
                    raise AssertionError("isolated Blims select-world failed")
                # Startup healthchecks keep their normal timeout; the short
                # operation deadline is selected only after isolated creation.
                config.write_text(config.read_text().replace("request_timeout_secs = 2", "request_timeout_secs = 5"))
            command = sandbox + base + plan.get("args", [])
            if plan.get("fixture") == "completion":
                status = subprocess.run(sandbox + base + ["model", "status"], env=env, cwd=work,
                                        check=True, capture_output=True, timeout=60)
                (work / "model-status.txt").write_bytes(status.stdout)
                if b"bcode.fake-provider" not in status.stdout or b"fake-echo" not in status.stdout:
                    raise AssertionError("completion fixture did not select the offline fake provider")
                created = subprocess.run(sandbox + base + ["session", "create", "completion-pty"],
                                         env=env, cwd=work / "repo", check=True, capture_output=True, timeout=60)
                session_id = created.stdout.decode().strip()
                command = sandbox + base + ["tui", session_id]
            run_pty(command, env, work, plan["steps"], options.grid_probe,
                    options.timeout, plan.get("exit_code", 0), daemon,
                    sandbox + base + ["--request-timeout-secs", "10", "session", "list",
                                      "--cwd", str(work / "repo"), "--json", "--with-status"])
    finally:
        if plan.get("fixture") == "blims" and daemon is not None and daemon.poll() is None:
            daemon.send_signal(signal.SIGCONT)
        stop(daemon)
        if options.cold_start:
            # Public application stop is pinned to this explicit artifact/state;
            # never inspect or signal an inferred daemon PID.
            result = subprocess.run(sandbox + base + ["server", "stop"],
                                    env=env, cwd=work, capture_output=True, timeout=60)
            (work / "cold-daemon-stop.txt").write_bytes(result.stdout + result.stderr)
            if result.returncode:
                raise AssertionError("isolated cold-start daemon cleanup failed")
    print(f"PASS: {plan.get('name', options.case or 'quick-cancel')} (only specified steps); artifacts: {work}")


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
