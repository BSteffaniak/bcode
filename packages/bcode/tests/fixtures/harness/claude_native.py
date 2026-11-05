#!/usr/bin/python3
"""Managed Claude subprocess double. No provider, network, or real native auth.

Scenario and effect oracle live in the explicit CLAUDE_CONFIG_DIR. Fixed-label
files are test oracles only. Never persist incoming frames or callback material.
"""
import json
import os
from pathlib import Path
import select
import sys
import time

VERSION = "bcode-offline-claude-fixture 1.0.0"
LIMIT = 65536


def emit(value):
    data = json.dumps(value, separators=(",", ":"))
    if len(data.encode()) > LIMIT:
        raise ValueError()
    print(data, flush=True)


def receive(timeout_seconds=15):
    data = bytearray()
    deadline = time.monotonic() + timeout_seconds
    while len(data) <= LIMIT:
        left = deadline - time.monotonic()
        if left <= 0 or not select.select([sys.stdin], [], [], left)[0]:
            raise ValueError()
        byte = os.read(0, 1)
        if not byte:
            raise EOFError()
        if byte == b"\n":
            return json.loads(data)
        data.extend(byte)
    raise ValueError()


def effect(root, label):
    try:
        with open(root / (label + ".effect"), "x", encoding="utf-8") as file:
            file.write("observed\n")
            file.flush()
            os.fsync(file.fileno())
    except FileExistsError:
        (root / (label + ".duplicate")).touch()
        raise ValueError()


def mark(root, label):
    # Fixed-label timing only: no frames, credentials or account data.
    (root / (label + ".monotonic_ns")).write_text(str(time.monotonic_ns()), encoding="ascii")


def main():
    if sys.argv[1:] == ["--version"]:
        print(VERSION)
        return
    if sys.argv[1:] == ["auth", "status"]:
        root = Path(os.environ["CLAUDE_CONFIG_DIR"])
        ready = not (root / "auth-login-required").exists()
        emit({"loggedIn":ready,"authMethod":"claude.ai" if ready else "none",
              "configDirectory":str(root),
              "diagnostic":"fixture-secret-DO-NOT-PERSIST"})
        raise SystemExit(0 if ready else 1)
    if sys.argv[1:] == ["auth", "login"]:
        root = Path(os.environ["CLAUDE_CONFIG_DIR"])
        if (root / "scenario").read_text(encoding="ascii").strip() != "terminal_auth":
            (root / "unexpected-auth-login").touch()
            raise SystemExit(86)
        if not sys.stdin.isatty() or not sys.stdout.isatty():
            raise ValueError()
        effect(root, "terminal-login")
        # Only the explicitly configured offline scenario simulates native login.
        # No browser, account, socket, credential store or provider is consulted.
        print("OFFLINE NATIVE TERMINAL fixture-secret-DO-NOT-PERSIST", flush=True)
        # Outlast the competitor's 40-second startup phase, but remain bounded
        # independently of the driver's 120-second total watchdog.
        deadline = time.monotonic() + 90
        while not (root / "terminal-login.release").exists():
            if time.monotonic() >= deadline:
                (root / "terminal-login.wait-expired").touch()
                raise ValueError()
            time.sleep(0.01)
        (root / "auth-login-required").unlink()
        (root / "terminal-login.finished").touch()
        return
    root = Path(os.environ["CLAUDE_CONFIG_DIR"])
    scenario = (root / "scenario").read_text(encoding="utf-8").strip()
    if scenario not in {"question", "lost_ack", "lost_before_confirmation", "sensitive_error", "cancel", "terminal_auth"}:
        raise ValueError()
    # Count only actual stream processes; version/auth inspection is not creation.
    for sequence in range(32):
        try:
            with open(root / ("launch-" + str(sequence)), "x", encoding="utf-8") as marker:
                marker.write("observed\n")
            break
        except FileExistsError:
            continue
    else:
        raise ValueError()
    mark(root, "stream-started")
    # The library supplies a retained native UUID on new and resume paths.
    session = next(arg.split("=", 1)[1] for arg in sys.argv
                   if arg.startswith(("--session-id=", "--resume=")))
    frame = receive()
    if frame.get("request", {}).get("subtype") != "initialize":
        raise ValueError()
    mark(root, "initialize-received")
    # Real initialize negotiation does not confirm a native session identity.
    # A pre-turn identity waiter deadlocks here and must fail the application
    # acceptance deadline instead of receiving fabricated fixture confirmation.
    emit({"type":"control_response", "response":{"subtype":"success",
          "request_id":frame["request_id"], "response":{}}})
    mark(root, "initialize-replied")
    (root / "initialized.waiting").touch()
    idle_seconds = 15
    timeout_file = root / "application-idle-seconds"
    if timeout_file.exists():
        idle_seconds = int(timeout_file.read_text(encoding="ascii"))
        if not 15 <= idle_seconds <= 120:
            raise ValueError()
    for _ in range(32):
        frame = receive(idle_seconds)
        if frame.get("type") == "user":
            # Only the actual admitted first user input may trigger native init.
            if frame.get("message", {}).get("content") != "fixture-only":
                (root / "unexpected-user.rejected").touch()
                raise ValueError()
            mark(root, "first-user-received")
            effect(root, "submit")
            if scenario == "lost_before_confirmation":
                raise SystemExit(83)
            emit({"type":"system", "subtype":"init", "session_id":session})
            (root / "binding-confirmed.emitted").touch()
            if scenario == "lost_ack":
                raise SystemExit(81)
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"running"})
            if scenario == "sensitive_error":
                # Deliberately unsafe native diagnostic: host must sanitize.
                emit({"type":"result", "subtype":"error_during_execution",
                      "is_error":True, "session_id":session,
                      "errors":["fixture-secret-DO-NOT-PERSIST"]})
                raise SystemExit(82)
            if scenario == "question":
                emit({"type":"control_request", "request_id":"native-question-1",
                      "request":{"subtype":"can_use_tool", "tool_name":"AskUserQuestion",
                      "input":{"questions":[{"question":"Choose fixture mode",
                      "multiSelect":False,"options":[{"label":"safe"},{"label":"inspect"}]}]},
                      "tool_use_id":"fixture-tool-1"}})
        elif frame.get("type") == "control_response":
            response = frame.get("response", {})
            if response.get("request_id") != "native-question-1":
                raise ValueError()
            effect(root, "respond")
            emit({"type":"assistant", "session_id":session, "turn_id":"fixture-turn-1",
                  "message":{"role":"assistant","content":[{"type":"text","text":"fixture complete"}]}})
            if (root / "hold-after-answer").exists():
                # Application cancellation targets an active continuation, not
                # a turn already authoritatively settled by result+idle.
                mark(root, "answer-continuation-running")
                emit({"type":"system", "subtype":"session_state_changed",
                      "session_id":session, "state":"running"})
                continue
            emit({"type":"result", "subtype":"success", "session_id":session,
                  "is_error":False, "result":"fixture complete"})
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"idle"})
        elif frame.get("request", {}).get("subtype") == "interrupt":
            effect(root, "interrupt")
            emit({"type":"control_response", "response":{"subtype":"success",
                  "request_id":frame["request_id"], "response":{}}})
            if (root / "hold-after-answer").exists():
                emit({"type":"result", "subtype":"success", "session_id":session,
                      "is_error":False, "result":"fixture interrupted"})
                emit({"type":"system", "subtype":"session_state_changed",
                      "session_id":session, "state":"idle"})
        else:
            raise ValueError()


if __name__ == "__main__":
    try:
        main()
    except EOFError:
        pass
    except (ValueError, KeyError, IndexError, OSError, TypeError, StopIteration):
        print("offline native protocol rejected", file=sys.stderr, flush=True)
        raise SystemExit(65)
