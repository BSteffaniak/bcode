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

def emit(value):
    print(json.dumps(value), flush=True)

def result(request, value):
    emit({"id": request["id"], "result": value})

def turn(status):
    return {"id": "turn-1", "status": status, "items": [],
            "error": {"message": "sensitive native error", "codexErrorInfo": "other"} if status == "failed" else None}

def completed(status="completed"):
    emit({"method": "turn/completed", "params": {"threadId": "native-thread", "turn": turn(status)}})

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    with open("methods.jsonl", "a", encoding="utf-8") as log:
        log.write(json.dumps({"method": method, "expectedTurnId": params.get("expectedTurnId"),
                              "callback": request.get("id") if method is None else None,
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
            if not local_only or not first_input:
                sys.exit(9)
            first_input = False
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
        if text == "overflow":
            for index in range(300):
                emit({"id": 100 + index, "method": "item/tool/requestUserInput", "params": {
                    "threadId": "native-thread", "turnId": "turn-1", "questions": []}})
        if text in ("approval", "question", "assistance", "dynamic", "foreign"):
            callback_method = "item/commandExecution/requestApproval"
            callback_params = {"threadId": "native-thread", "turnId": "turn-1", "itemId": "item-1", "command": "echo fixture"}
            if text == "foreign":
                callback_params["threadId"] = "unrelated-thread"
            if text == "question":
                callback_method = "item/tool/requestUserInput"
                callback_params["questions"] = [{"id": "q1", "question": "Choose one", "isSecret": False}]
            if text in ("assistance", "dynamic"):
                callback_method = "item/tool/call"
                callback_params.update({"tool": "bcode_parent_assistance" if text == "assistance" else "shell.run",
                                        "arguments": {"question": "Need context"}, "callId": "native-call"})
            emit({"id": 91, "method": callback_method, "params": callback_params})
        emit({"method": "item/agentMessage/delta", "params": {"threadId": "native-thread", "turnId": "turn-1", "delta": "fixture text"}})
        if text not in ("approval", "question", "assistance", "dynamic"):
            completed(text if text in ("failed", "interrupted") else "completed")
    elif method == "turn/interrupt":
        result(request, {})
        completed("interrupted")
    elif method is None:
        completed()
    else:
        emit({"id": request["id"], "error": {"code": -32601, "message": "sensitive native error"}})
