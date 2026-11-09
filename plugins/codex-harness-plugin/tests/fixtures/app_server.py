#!/usr/bin/env python3
"""Offline double for Codex 8553558345. No native agent/provider invocation."""
import json
import os
import sys
import time

if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.0.0-pinned-fixture")
    sys.exit(0)
if "--version" in sys.argv:
    sys.exit(2)
ready = False
local_only = False
first_input = True
terminal_inventory = []
turn_count = 0
current_turn = "turn-1"

def emit(value):
    print(json.dumps(value), flush=True)

def result(request, value):
    emit({"id": request["id"], "result": value})

def turn(status):
    return {"id": current_turn, "status": status, "items": [],
            "error": {"message": "sensitive native error", "codexErrorInfo": "other"} if status == "failed" else None}

def marker(text="result-fixture-end", item_id="marker"):
    # A completed commentary message is durable display text; deltas are live-only.
    emit({"method": "item/completed", "params": {"threadId": "native-thread", "turnId": current_turn,
          "item": {"type": "agentMessage", "id": item_id, "text": text, "phase": "commentary"}}})

def completed(status="completed"):
    emit({"method": "turn/completed", "params": {"threadId": "native-thread", "turn": turn(status)}})

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    with open("methods.jsonl", "a", encoding="utf-8") as log:
        log.write(json.dumps({"method": method, "expectedTurnId": params.get("expectedTurnId"),
                              "callback": request.get("id") if method is None else None,
                              "turnId": params.get("turnId"), "threadId": params.get("threadId"),
                              "result": request.get("result"),
                              "success": request.get("result", {}).get("success")}) + "\n")
    if method == "initialize":
        result(request, {"codexHome": "/wrong-home" if os.path.exists("wrong-home") else os.environ["CODEX_HOME"],
                         "userAgent": "bcode/0.0.0 (Linux; fixture) mutable-user-agent", "platformFamily": "unix", "platformOs": "linux"})
    elif method == "initialized":
        pass
    elif method == "config/read":
        result(request, {"config": {"cli_auth_credentials_store": "file" if os.path.exists("wrong-store") else "ephemeral",
                                    "model_provider": "openai", "features": {"multi_agent": False}}})
    elif method == "account/read":
        if os.path.exists("fail-status") and ready:
            emit({"id": request["id"], "error": {"code": -32000, "message": "sensitive status failure"}})
        else:
            result(request, {"account": {"type": "apiKey"} if ready else None, "requiresOpenaiAuth": True})
    elif method == "account/login/start":
        if params["type"] == "apiKey":
            ready = True
            result(request, {"type": "apiKey"})
        else:
            result(request, {"type": params["type"], "loginId": "native-login",
                             "authUrl": "https://fixture.invalid/sensitive", "verificationUrl": "https://fixture.invalid/device",
                             "userCode": "sensitive-code"})
            ready = True
            if not os.path.exists("pending-login"):
                emit({"method": "account/login/completed", "params": {"loginId": "native-login", "success": True,
                                                                         "error": "must-not-persist"}})
    elif method == "account/login/cancel":
        result(request, {"status": "notFound" if os.path.exists("cancel-not-found") else "canceled"})
    elif method == "remoteControl/disable":
        if params != {"ephemeral": True}:
            sys.exit(10)  # Must not change persisted user preference.
        local_only = not os.path.exists("remote-refuses-disable")
        result(request, {"status": "disabled" if local_only else "connected"})
    elif method == "remoteControl/status/read":
        result(request, {"status": "disabled" if local_only else "connected"})
    elif method in ("thread/start", "thread/resume", "thread/read"):
        if not local_only:
            sys.exit(11)  # Must establish exclusivity before native creation/adoption.
        if method == "thread/resume" and params.get("excludeTurns") is not True:
            sys.exit(3)
        if method == "thread/read" and params.get("includeTurns") is not False:
            sys.exit(4)
        if method == "thread/start" and params.get("daybreakEnabled") is not False:
            sys.exit(12)
        if method == "thread/resume":
            first_input = False
        thread = {"id": params.get("threadId", "native-thread"), "status": {"type": "idle"}, "turns": [], "daybreakEnabled": False}
        result(request, {"thread": thread})
        if method != "thread/read":
            emit({"method": "thread/started", "params": {"thread": thread}})
        if os.path.exists("remote-activates"):
            emit({"method": "remoteControl/status/changed", "params": {"status": "connected"}})
        if os.path.exists("stop-reading"):
            time.sleep(60)
    elif method in ("turn/start", "turn/steer"):
        text = params["input"][0]["text"]
        if method == "turn/start":
            if not local_only:
                sys.exit(9)
            if not first_input and os.path.exists("later-start-forbidden"):
                sys.exit(9)
            first_input = False
            turn_count += 1
            current_turn = f"turn-{turn_count}"
        if method == "turn/steer" and turn_count == 0:
            turn_count = 1  # The fixture's first (steered) turn is turn-1.
        if method == "turn/steer" and params.get("expectedTurnId") != "turn-1":
            emit({"id": request["id"], "error": {"code": -32600, "message": "expected turn mismatch"}})
            continue
        if text == "wrong-ack-shape":
            result(request, {"turnId": "turn-1"} if method == "turn/start" else {"turn": turn("inProgress")})
            continue
        if text == "lost-ack":
            sys.exit(0)
        if text == "stderr":
            sys.stderr.write("x" * 262144)
            sys.stderr.flush()
        result(request, {"turn": turn("inProgress")} if method == "turn/start" else {"turnId": "turn-1"})
        emit({"method": "turn/started", "params": {"threadId": "native-thread", "turn": turn("inProgress")}})
        if text.startswith("lifecycle-"):
            def item_event(method, item, turn_id="turn-1", thread_id="native-thread"):
                emit({"method": method, "params": {"threadId": thread_id, "turnId": turn_id, "item": item}})
            if text == "lifecycle-inventory":
                terminal_inventory = [{"itemId": f"inventory-{i}", "processId": str(1000+i),
                                       "command": "fixture only", "cwd": os.getcwd()} for i in range(40)]
            command = {"type": "commandExecution", "id": "command-live", "processId": "42",
                       "command": "fixture only", "cwd": os.getcwd(), "status": "inProgress",
                       "commandActions": [], "aggregatedOutput": None, "exitCode": None, "durationMs": None}
            item_event("item/started", command)
            item_event("item/started", {"type": "mcpToolCall", "id": "persistent-tool", "status": "inProgress"})
            item_event("item/completed", {"type": "agentMessage", "id": "final", "phase": "final_answer", "text": "foreground complete"})
            completed()
            # Work may complete after foreground terminal; no new Bcode submission.
            command["status"] = "completed"
            command["exitCode"] = 0
            item_event("item/completed", command, thread_id="foreign" if text == "lifecycle-foreign" else "native-thread")
            if text == "lifecycle-race":
                followup = {"id": "automatic-turn", "rootTurnId": "turn-1", "status": "inProgress", "items": [], "error": None}
                emit({"method": "turn/started", "params": {"threadId": "native-thread", "turn": followup}})
                # A late primary answer cannot overwrite the closed foreground output.
                item_event("item/completed", {"type": "agentMessage", "id": "late", "phase": "final_answer", "text": "wrong late answer"})
                item_event("item/completed", {"type": "agentMessage", "id": "followup", "phase": "final_answer", "text": "followup complete"}, "automatic-turn")
                followup["status"] = "completed"
                emit({"method": "turn/completed", "params": {"threadId": "native-thread", "turn": followup}})
                # Duplicate running notification must not reopen terminal work.
                command["status"] = "inProgress"
                item_event("item/started", command)
            continue
        if text == "overflow":
            for index in range(1100):
                emit({"id": 100 + index, "method": "item/tool/requestUserInput", "params": {
                    "threadId": "native-thread", "turnId": "turn-1", "questions": []}})
        if text.startswith("terminal-"):
            status = text.removeprefix("terminal-")
            native_turn = turn(status if status in ("completed", "failed", "interrupted") else "completed")
            native_turn.update({"itemsView": "summary", "rootTurnId": "turn-1",
                                "startedAt": 1700000000, "completedAt": 1700000001, "durationMs": 1000})
            thread_id = "native-thread"
            if status == "wrong-turn":
                native_turn["id"] = "other-turn"
            if status == "foreign":
                thread_id = "foreign-thread"
            if status == "invalid":
                native_turn["status"] = "future-status"
            if status == "contradictory":
                native_turn["error"] = {"message": "sensitive native error", "codexErrorInfo": "other"}
            if status == "background":
                native_turn["items"] = [{"type": "commandExecution", "id": "command-1", "command": "sleep 60",
                                         "cwd": os.getcwd(), "processId": "123", "status": "inProgress",
                                         "commandActions": [], "aggregatedOutput": None, "exitCode": None, "durationMs": None}]
            if status == "lost":
                sys.exit(0)
            emit({"method": "turn/completed", "params": {"threadId": thread_id, "turn": native_turn}})
            if status == "duplicate":
                emit({"method": "turn/completed", "params": {"threadId": thread_id, "turn": native_turn}})
            marker()
            continue
        if text.startswith("result-"):
            emit({"method": "item/agentMessage/delta", "params": {
                "threadId": "native-thread", "turnId": "turn-1", "itemId": "message-1",
                "delta": "partial streamed text"}})
            item = {"type": "agentMessage", "id": "message-1", "text": "Complete answer: 界 👋",
                    "phase": "final_answer", "memoryCitation": None, "delivery": None, "questions": None}
            item_params = {"threadId": "native-thread", "turnId": "turn-1",
                           "item": item, "completedAtMs": 1700000000000}
            item_method = "item/completed"
            if text == "result-commentary":
                item["phase"] = "commentary"
            if text == "result-partial":
                item["phase"] = "partial_answer"
            if text == "result-unphased":
                item["phase"] = None
            if text == "result-unknown-phase":
                item["phase"] = "future_phase"
            if text == "result-started":
                item_method = "item/started"
            if text == "result-unaccepted-turn":
                unaccepted = turn("inProgress")
                unaccepted["id"] = "other-turn"
                emit({"method": "turn/started", "params": {"threadId": "native-thread", "turn": unaccepted}})
                item_params["turnId"] = "other-turn"
            if text == "result-wrong-turn":
                item_params["turnId"] = "other-turn"
            if text == "result-missing-turn":
                del item_params["turnId"]
            if text == "result-foreign-thread":
                item_params["threadId"] = "unrelated-thread"
            if text == "result-oversized":
                item["text"] = "x" * 65537
            if text == "result-malformed":
                del item["text"]
            if text == "result-late":
                completed()
            if text != "result-delta-only":
                emit({"method": item_method, "params": item_params})
            if text not in ("result-held", "result-late", "result-oversized", "result-malformed"):
                completed("interrupted" if text == "result-interrupted" else "failed" if text == "result-failed" else "completed")
            marker()
            continue
        if text in ("approval", "question", "multi-question", "duplicate-question", "secret-question", "resolved-question", "foreign-resolved-question", "assistance", "dynamic", "foreign"):
            callback_method = "item/commandExecution/requestApproval"
            callback_params = {"threadId": "native-thread", "turnId": "turn-1", "itemId": "item-1", "command": "echo fixture"}
            if text == "foreign":
                callback_params["threadId"] = "unrelated-thread"
            if text in ("question", "multi-question", "duplicate-question", "secret-question", "resolved-question", "foreign-resolved-question"):
                callback_method = "item/tool/requestUserInput"
                callback_params["questions"] = [{"id": "q1", "question": "Choose one", "isSecret": False,
                                                  "options": [{"label": "First", "description": "First choice"}]}]
                if text in ("multi-question", "duplicate-question", "secret-question"):
                    callback_params["questions"].append({"id": "q1" if text == "duplicate-question" else "q2",
                                                        "question": "Explain why", "isSecret": text == "secret-question"})
            if text in ("assistance", "dynamic"):
                callback_method = "item/tool/call"
                callback_params.update({"tool": "bcode_parent_assistance" if text == "assistance" else "shell.run",
                                        "arguments": {"question": "Need context"}, "callId": "native-call"})
            emit({"id": 91, "method": callback_method, "params": callback_params})
            if text in ("resolved-question", "foreign-resolved-question"):
                emit({"method": "serverRequest/resolved", "params": {
                    "threadId": "unrelated-thread" if text == "foreign-resolved-question" else "native-thread",
                    "requestId": 91}})
        emit({"method": "item/agentMessage/delta", "params": {"threadId": "native-thread", "turnId": current_turn, "itemId": "fixture-text", "delta": "fixture "}})
        emit({"method": "item/agentMessage/delta", "params": {"threadId": "native-thread", "turnId": current_turn, "itemId": "fixture-text", "delta": "text"}})
        marker("fixture text", "fixture-text")
        if text not in ("approval", "question", "multi-question", "duplicate-question", "secret-question", "resolved-question", "foreign-resolved-question", "assistance", "dynamic"):
            completed(text if text in ("failed", "interrupted") else "completed")
    elif method == "thread/backgroundTerminals/list":
        start = int(params.get("cursor") or "0")
        end = min(start + int(params.get("limit", 4)), len(terminal_inventory))
        result(request, {"data": terminal_inventory[start:end],
                         "nextCursor": str(end) if end < len(terminal_inventory) else None})
    elif method == "turn/interrupt":
        if os.path.exists("lost-interrupt-ack"):
            sys.exit(0)
        result(request, {})
        completed("interrupted")
    elif method is None:
        completed()
    else:
        emit({"id": request["id"], "error": {"code": -32601, "message": "sensitive native error"}})
