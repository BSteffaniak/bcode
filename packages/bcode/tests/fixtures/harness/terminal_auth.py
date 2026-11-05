#!/usr/bin/python3
"""Bounded PTY acceptance driver; keeps native terminal secrets in memory only."""
import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import time

LIMIT = 1048576
SECRET = b"fixture-secret-DO-NOT-PERSIST"
STARTED = time.monotonic()


class AcceptanceFailure(Exception):
    """Only caller-authored fixed condition codes may cross the transcript boundary."""


def diagnostic(stage, exit_code=None):
    # Call sites supply literals only. Never accept native text, exceptions,
    # decoded snapshots, argv, paths, flow handles, or callback material here.
    value = {"stage": stage, "elapsed_ms": int((time.monotonic() - STARTED) * 1000)}
    if exit_code is not None:
        value["exit_code"] = int(exit_code)
    print("offline-terminal " + json.dumps(value), file=sys.stderr, flush=True)


def fail(condition):
    diagnostic(condition)
    raise AcceptanceFailure()


def start(argv):
    master, slave = pty.openpty()
    try:
        child = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave,
                                 close_fds=True, start_new_session=True)
    finally:
        os.close(slave)
    return child, master, bytearray()


def drain(item):
    _, fd, data = item
    if not select.select([fd], [], [], 0.01)[0]:
        return
    try:
        chunk = os.read(fd, 8192)
    except OSError as error:
        if error.errno == errno.EIO:
            return
        raise
    data.extend(chunk)
    if len(data) > LIMIT:
        fail("output_bound_exceeded")


def finish(item, deadline):
    child, _, _ = item
    while child.poll() is None:
        drain(item)
        if time.monotonic() >= deadline:
            fail("child_deadline_exceeded")
    # Drain bounded remaining pipe/PTY data after child exit, never print it.
    for _ in range(130):
        drain(item)
    return child.returncode


def snapshot(data):
    text = data.decode("utf-8").replace("\r\n", "\n")
    decoder = json.JSONDecoder()
    found = []
    for index, char in enumerate(text):
        if char == "{":
            try:
                value, _ = decoder.raw_decode(text[index:])
            except ValueError:
                continue
            if isinstance(value, dict) and "phase" in value and "readiness" in value:
                found.append(value)
    if len(found) != 1:
        fail("snapshot_missing_or_nonunique")
    return found[0]


def cli_failure(data, stage):
    # Consume only the supported typed CLI diagnostic envelope; never infer a
    # failure by matching human-readable native/CLI output. Native text remains
    # untrusted even when it resembles JSON. Only local allowlisted labels exit.
    allowed = {
        "invalid_arguments", "connection_timeout", "request_timeout",
        "daemon_incompatible", "ipc_protocol_failed", "unexpected_response",
        "unexpected_envelope", "unclassified_application_error",
        "unclassified_cli_error", "daemon_start_failed", "ipc_transport_failed",
        "ipc_decode_failed", "daemon_readiness_notification_timeout",
        # Exact host-owned codes only; lane 11 owns their typed CLI mapping.
        "managed_session_invalid", "managed_session_storage_unavailable",
        "managed_session_stale_owner", "managed_session_conflict",
        "managed_session_uncertain", "managed_session_unavailable",
        "managed_session_deadline", "managed_session_unsupported",
        "managed_session_scope_mismatch", "managed_session_sensitive_transport_required",
    }
    phases = set()
    allowed_phases = {
        "application", "managed_session", "auth_prepare", "auth_status",
        "auth_handoff", "auth_reaped", "auth_continue", "auth_acknowledge",
    }
    codes = set()
    for line in bytes(data).splitlines():
        if len(line) > 4096 or not line.startswith(b"{"):
            continue
        try:
            value = json.loads(line)
        except (ValueError, UnicodeError):
            continue
        if isinstance(value, dict) and value.get("schema_version") == 1:
            code = value.get("code")
            if isinstance(code, str) and code in allowed:
                codes.add(code)
                phase = value.get("phase")
                if isinstance(phase, str) and phase in allowed_phases:
                    phases.add(phase)
    diagnostic(stage)
    if len(phases) == 1:
        diagnostic("typed_cli_phase:" + next(iter(phases)))
    else:
        diagnostic("typed_cli_phase:missing_or_ambiguous")
    if len(codes) == 1:
        diagnostic("typed_cli_code:" + next(iter(codes)))
    else:
        diagnostic("typed_cli_code:missing_or_ambiguous")


def main():
    root = Path(sys.argv[1])
    executable = Path(sys.argv[2])
    if not executable.is_absolute():
        fail("executable_not_absolute")
    argv = [str(executable), "--json-errors", *sys.argv[3:]]
    children = []
    total_deadline = time.monotonic() + 120
    owner_spawn_deadline = min(total_deadline, time.monotonic() + 40)
    try:
        diagnostic("owner_start")
        first = start(argv)
        children.append(first)
        while not (root / "terminal-login.effect").exists():
            drain(first)
            code = first[0].poll()
            if code is not None:
                diagnostic("owner_exit_before_native_effect", code)
                cli_failure(first[2], "owner_typed_failure")
                fail("native_terminal_not_launched")
            if time.monotonic() >= owner_spawn_deadline:
                fail("native_effect_deadline_exceeded")
        diagnostic("native_effect_observed")
        # Same namespace, separate real client terminal: a second prepare/claim
        # must fail while the first client retains the unreaped native child.
        diagnostic("competitor_start")
        second = start(argv)
        children.append(second)
        diagnostic("competitor_wait")
        competitor_deadline = min(total_deadline, time.monotonic() + 40)
        code = finish(second, competitor_deadline)
        diagnostic("competitor_exit", code)
        if code != 0:
            cli_failure(second[2], "competitor_typed_failure")
        if code == 0:
            fail("duplicate_terminal_admitted")
        if SECRET in second[2]:
            fail("competitor_secret_disclosure")
        if (root / "terminal-login.duplicate").exists():
            fail("duplicate_native_effect")
        code = first[0].poll()
        if code is not None:
            diagnostic("owner_exit_before_release", code)
            fail("native_owner_not_live_at_release")
        diagnostic("native_release")
        (root / "terminal-login.release").touch()
        diagnostic("owner_wait")
        owner_completion_deadline = min(total_deadline, time.monotonic() + 30)
        code = finish(first, owner_completion_deadline)
        diagnostic("owner_exit", code)
        if code != 0:
            cli_failure(first[2], "owner_typed_failure")
            if (root / "terminal-login.finished").exists():
                diagnostic("native_fixture_finished_before_owner_failure")
            fail("owner_handoff_failed")
        if SECRET not in first[2]:
            fail("dedicated_output_missing")
        diagnostic("snapshot_decode")
        value = snapshot(first[2])
        if value["phase"] != "settled" or value["readiness"] != "ready":
            fail("reinspection_not_settled_ready")
        if not (root / "terminal-login.finished").exists():
            fail("native_finish_marker_missing")
        if (root / "terminal-login.duplicate").exists():
            fail("duplicate_native_effect")
        diagnostic("complete")
        # No native transcript, flow handle or callback leaves this driver.
        print(json.dumps({"terminal_ready": True, "duplicate_rejected": True,
                          "native_executions": 1, "dedicated_output_seen": True}))
    finally:
        if (root / "terminal-login.wait-expired").exists():
            diagnostic("native_fixture_release_wait_expired")
        for child, fd, data in children:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=2)
            os.close(fd)
            data.clear()


if __name__ == "__main__":
    try:
        main()
    except AcceptanceFailure:
        print("offline terminal acceptance failed (transcript withheld)", file=sys.stderr)
        raise SystemExit(1)
    except Exception:
        # Never stringify exceptions: native text, paths and callback material
        # may be embedded even when the exception is not an explicit assertion.
        diagnostic("unexpected_exception")
        print("offline terminal acceptance failed (transcript withheld)", file=sys.stderr)
        raise SystemExit(1)
