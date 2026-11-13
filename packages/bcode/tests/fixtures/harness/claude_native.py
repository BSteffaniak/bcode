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
import uuid

VERSION = "2.1.308 (Claude Code)"
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


def wait_for(root, label, seconds=60):
    # Bounded test-controlled pause; never consults native state or credentials.
    deadline = time.monotonic() + seconds
    while not (root / label).exists():
        if time.monotonic() >= deadline:
            (root / (label + ".wait-expired")).touch()
            raise ValueError()
        time.sleep(0.01)


def stream_event(session, event):
    emit({"type":"stream_event", "session_id":session, "uuid":str(uuid.uuid4()),
          "parent_tool_use_id":None, "event":event})


def rich_turn(root, session, submission_uuid, content):
    """Partial-message streaming, thinking, one native tool and its result.

    Enabled only by the explicit `rich-output` oracle so ordinary scenarios keep
    their exact frame sequence. All text is fixture display content.
    """
    message_id = "msg_fixture_" + content
    if (root / "gate-stream").exists():
        # Lets a held observation be waiting before any native output for this turn exists.
        mark(root, "stream-gated")
        wait_for(root, "open-stream")
    stream_event(session, {"type":"message_start", "message":{"id":message_id,
                 "type":"message", "role":"assistant", "content":[],
                 "model":"claude-fixture", "usage":{"input_tokens":12, "output_tokens":0}}})
    stream_event(session, {"type":"content_block_start", "index":0,
                 "content_block":{"type":"thinking", "thinking":"", "signature":""}})
    stream_event(session, {"type":"content_block_delta", "index":0,
                 "delta":{"type":"thinking_delta", "thinking":"Considering " + content}})
    stream_event(session, {"type":"content_block_stop", "index":0})
    stream_event(session, {"type":"content_block_start", "index":1,
                 "content_block":{"type":"text", "text":""}})
    stream_event(session, {"type":"content_block_delta", "index":1,
                 "delta":{"type":"text_delta", "text":"Streaming "}})
    stream_event(session, {"type":"content_block_delta", "index":1,
                 "delta":{"type":"text_delta", "text":"reply " + content}})
    mark(root, "stream-deltas-emitted")
    if (root / "hold-stream").exists():
        # Lets a held observation return the live sidecar before durable text exists.
        wait_for(root, "release-stream")
    stream_event(session, {"type":"content_block_stop", "index":1})
    stream_event(session, {"type":"content_block_start", "index":2,
                 "content_block":{"type":"tool_use", "id":"toolu_fixture_" + content,
                                  "name":"Read", "input":{}}})
    stream_event(session, {"type":"content_block_delta", "index":2,
                 "delta":{"type":"input_json_delta",
                          "partial_json":"{\"file_path\":\"/fixture/notes.txt\"}"}})
    stream_event(session, {"type":"content_block_stop", "index":2})
    stream_event(session, {"type":"message_delta", "delta":{"stop_reason":"tool_use"},
                 "usage":{"output_tokens":34}})
    stream_event(session, {"type":"message_stop"})
    emit({"type":"assistant", "session_id":session, "uuid":str(uuid.uuid4()),
          "user_message_uuid":submission_uuid,
          "message":{"id":message_id, "role":"assistant", "model":"claude-fixture",
                     "content":[
                         {"type":"thinking", "thinking":"Considering " + content,
                          "signature":"fixture-signature"},
                         {"type":"text", "text":"Streaming reply " + content},
                         {"type":"tool_use", "id":"toolu_fixture_" + content, "name":"Read",
                          "input":{"file_path":"/fixture/notes.txt"}}]}})
    if (root / ("approve-" + content)).exists():
        # Like the real CLI in default permission mode: the native tool waits for an
        # explicit host decision before it runs and produces a result.
        await_tool_approval(root, session, content)
    emit({"type":"user", "session_id":session, "uuid":str(uuid.uuid4()),
          "parent_tool_use_id":None,
          "message":{"role":"user", "content":[
              {"type":"tool_result", "tool_use_id":"toolu_fixture_" + content,
               "content":"fixture notes", "is_error":False}]}})


def answer_mcp_control(frame):
    """Reply to host MCP status/notification control requests; False if not one."""
    request = frame.get("request", {}) if frame.get("type") == "control_request" else {}
    if request.get("subtype") == "mcp_status":
        emit({"type":"control_response", "response":{"subtype":"success",
              "request_id":frame["request_id"], "response":{"mcpServers":[
                  {"name":"bcode-tools", "status":"connected"}]}}})
        return True
    if (request.get("subtype") == "mcp_message"
            and request.get("server_name") == "bcode-tools"
            and request.get("message", {}).get("method")
                == "notifications/tools/list_changed"):
        # Acknowledge notification receipt only, never invent catalog completion.
        emit({"type":"control_response", "response":{"subtype":"success",
              "request_id":frame["request_id"], "response":{}}})
        return True
    return False


def await_tool_approval(root, session, content):
    """Request native tool permission and require an explicit allow for this exact tool."""
    request_id = "native-permission-" + content
    emit({"type":"control_request", "request_id":request_id,
          "request":{"subtype":"can_use_tool", "tool_name":"Read",
                     "input":{"file_path":"/fixture/notes.txt"},
                     "tool_use_id":"toolu_fixture_" + content}})
    mark(root, "permission-requested")
    for _ in range(32):
        frame = receive(120)
        if answer_mcp_control(frame):
            continue
        response = frame.get("response", {}) if frame.get("type") == "control_response" else {}
        if response.get("request_id") != request_id or response.get("subtype") != "success":
            raise ValueError()
        decision = response.get("response", {})
        if (decision.get("behavior") != "allow"
                or decision.get("updatedInput") != {"file_path":"/fixture/notes.txt"}):
            (root / ("approve-" + content + ".denied")).touch()
            raise ValueError()
        effect(root, "approved-" + content)
        return
    raise ValueError()


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
    if scenario not in {"question", "approval", "lost_ack", "lost_before_confirmation", "sensitive_error", "cancel", "terminal_auth", "multi_turn"}:
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
    # Like the real CLI, initialize advertises the model keys mid-session switching accepts.
    emit({"type":"control_response", "response":{"subtype":"success",
          "request_id":frame["request_id"], "response":{"models":[
              {"value":"claude-fixture", "displayName":"Claude Fixture"}]}}})
    mark(root, "initialize-replied")
    (root / "initialized.waiting").touch()
    idle_seconds = 15
    timeout_file = root / "application-idle-seconds"
    if timeout_file.exists():
        idle_seconds = int(timeout_file.read_text(encoding="ascii"))
        if not 15 <= idle_seconds <= 120:
            raise ValueError()
    submission_uuid = None
    submission_uuid_seen = None
    for _ in range(32):
        frame = receive(idle_seconds)
        if frame.get("type") == "user" and scenario == "multi_turn":
            # Sequential foreground turns on one live process; each ends with its own
            # attributed result. Only the first turn confirms the native identity.
            content = frame.get("message", {}).get("content")
            if not isinstance(content, str) or not content.startswith("turn-"):
                (root / "unexpected-user.rejected").touch()
                raise ValueError()
            submission_uuid = str(uuid.UUID(frame["uuid"]))
            effect(root, content)
            if submission_uuid_seen is None:
                emit({"type":"system", "subtype":"init", "session_id":session})
                (root / "binding-confirmed.emitted").touch()
            submission_uuid_seen = submission_uuid
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"running"})
            rich = (root / "rich-output").exists()
            if rich:
                rich_turn(root, session, submission_uuid, content)
            emit({"type":"assistant", "session_id":session, "uuid":str(uuid.uuid4()),
                  "user_message_uuid":submission_uuid,
                  "message":{"role":"assistant",
                             "content":[{"type":"text","text":"reply " + content}]}})
            result = {"type":"result", "subtype":"success", "session_id":session,
                      "is_error":False, "result":"reply " + content, "uuid":str(uuid.uuid4()),
                      "user_message_uuid":submission_uuid, "queued_turn_count":0}
            if rich:
                result.update({"duration_ms":1500, "total_cost_usd":0.0012,
                               "usage":{"input_tokens":12, "output_tokens":34,
                                        "cache_read_input_tokens":5,
                                        "cache_creation_input_tokens":6}})
            emit(result)
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"idle"})
        elif (frame.get("type") == "user" and submission_uuid is not None
              and frame.get("message", {}).get("content") == "queued-follow-up"):
            # The follow-up a client sent while the answered turn was still running. The host
            # must hold it and deliver it exactly once, only after that turn ended.
            if not (root / "first-turn-finished").exists():
                (root / "follow-up-during-turn.rejected").touch()
                raise ValueError()
            follow_up = str(uuid.UUID(frame["uuid"]))
            effect(root, "queued-follow-up")
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"running"})
            emit({"type":"assistant", "session_id":session, "uuid":str(uuid.uuid4()),
                  "user_message_uuid":follow_up,
                  "message":{"role":"assistant","content":[{"type":"text","text":"follow-up reply"}]}})
            emit({"type":"result", "subtype":"success", "session_id":session,
                  "is_error":False, "result":"follow-up reply", "uuid":str(uuid.uuid4()),
                  "user_message_uuid":follow_up, "queued_turn_count":0})
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"idle"})
        elif frame.get("type") == "user":
            # Only the actual admitted first user input may trigger native init.
            if frame.get("message", {}).get("content") != "fixture-only":
                (root / "unexpected-user.rejected").touch()
                raise ValueError()
            submission_uuid = str(uuid.UUID(frame["uuid"]))
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
            emit({"type":"assistant", "session_id":session,
                  "uuid":"66666666-6666-4666-8666-666666666666",
                  "user_message_uuid":submission_uuid,
                  "message":{"role":"assistant", "content":[]}})
            if scenario == "sensitive_error":
                # Deliberately unsafe native diagnostic: host must sanitize.
                emit({"type":"result", "subtype":"error_during_execution",
                      "is_error":True, "session_id":session,
                      "uuid":"77777777-7777-4777-8777-777777777777",
                      "user_message_uuid":submission_uuid, "queued_turn_count":0,
                      "errors":["fixture-secret-DO-NOT-PERSIST"]})
                raise SystemExit(82)
            if scenario == "approval":
                # A native write the host's agent policy must decide without asking the user.
                emit({"type":"control_request", "request_id":"native-approval-1",
                      "request":{"subtype":"can_use_tool", "tool_name":"Write",
                      "input":{"file_path":"fixture.txt", "content":"fixture"},
                      "tool_use_id":"fixture-tool-2"}})
            if scenario == "question":
                emit({"type":"control_request", "request_id":"native-question-1",
                      "request":{"subtype":"can_use_tool", "tool_name":"AskUserQuestion",
                      "input":{"questions":[{"question":"Choose fixture mode",
                      "multiSelect":False,"options":[{"label":"safe"},{"label":"inspect"}]}]},
                      "tool_use_id":"fixture-tool-1"}})
        elif (frame.get("type") == "control_response"
              and frame.get("response", {}).get("request_id") == "native-approval-1"):
            behavior = frame["response"].get("response", {}).get("behavior")
            if behavior not in {"allow", "deny"}:
                raise ValueError()
            effect(root, "approval-" + behavior)
            emit({"type":"result", "subtype":"success", "session_id":session,
                  "is_error":False, "result":"approval " + behavior,
                  "uuid":str(uuid.uuid4()),
                  "user_message_uuid":submission_uuid, "queued_turn_count":0})
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"idle"})
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
            finish_after = root / "finish-after-answer-seconds"
            if finish_after.exists():
                # Keep the answered turn running long enough for a client to send more input.
                emit({"type":"system", "subtype":"session_state_changed",
                      "session_id":session, "state":"running"})
                time.sleep(int(finish_after.read_text(encoding="ascii")))
                (root / "first-turn-finished").touch()
            emit({"type":"result", "subtype":"success", "session_id":session,
                  "is_error":False, "result":"fixture complete",
                  "uuid":"88888888-8888-4888-8888-888888888888",
                  "user_message_uuid":submission_uuid, "queued_turn_count":0})
            emit({"type":"system", "subtype":"session_state_changed",
                  "session_id":session, "state":"idle"})
        elif answer_mcp_control(frame):
            pass
        elif (frame.get("type") == "control_request"
              and frame.get("request", {}).get("subtype") in {"set_model", "set_permission_mode"}):
            # Mid-session configuration: acknowledge and record a bounded fixed-label oracle.
            request = frame["request"]
            value = request.get("model") if request["subtype"] == "set_model" else request.get("mode")
            if not isinstance(value, str) or not value or len(value) > 64 or not value.replace("-", "").replace("_", "").replace(".", "").isalnum():
                raise ValueError()
            effect(root, request["subtype"].replace("_", "-") + "-" + value)
            emit({"type":"control_response", "response":{"subtype":"success",
                  "request_id":frame["request_id"], "response":{}}})
        elif frame.get("request", {}).get("subtype") == "interrupt":
            effect(root, "interrupt")
            emit({"type":"control_response", "response":{"subtype":"success",
                  "request_id":frame["request_id"], "response":{}}})
            if (root / "hold-after-answer").exists():
                emit({"type":"result", "subtype":"success", "session_id":session,
                      "is_error":False, "result":"fixture interrupted",
                      "uuid":"99999999-9999-4999-8999-999999999999",
                      "user_message_uuid":submission_uuid, "queued_turn_count":0})
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
