#!/usr/bin/env python3
"""Deterministic fake `agy` CLI + ACP stdio server for offline harness tests."""
import json
import os
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    args = sys.argv[1:]
    if args == ["--version"]:
        print("1.5.15")
        return 0
    if args == ["models"]:
        print("Fetching available models...")
        print("gemini-4-neon-high\tGemini 4 Neon (High)")
        print("gemini-4-neon-medium\tGemini 4 Neon (Medium)")
        print("gemini-3.8-flash-low\tGemini 3.8 Flash (Low)")
        return 0
    if not args or args[0] != "acp":
        sys.stderr.write(f"unsupported args: {args}\n")
        return 2

    gemini_dir = ""
    if "--gemini-dir" in args:
        idx = args.index("--gemini-dir")
        if idx + 1 < len(args):
            gemini_dir = args[idx + 1]

    unauth = os.path.exists(os.path.join(gemini_dir, "UNAUTH")) if gemini_dir else False
    session_id = "11111111-2222-4333-8444-555555555555"
    current_model = "gemini-4-neon-high"
    current_mode = "default"
    pending_perm = None  # (perm_rpc_id, prompt_rpc_id, prompt_text)
    # Set by session/cancel while a permission is pending: like a spec-compliant ACP agent,
    # the prompt only ends once the client answers that permission with `cancelled`.
    cancel_requested = False
    modes = {
        "currentModeId": "default",
        "availableModes": [
            {"id": "default", "name": "Default"},
            {"id": "accept-edits", "name": "Accept edits"},
            {"id": "plan", "name": "Plan"},
        ],
    }

    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        msg = json.loads(raw)

        # Reply to our inbound permission request
        if "method" not in msg and pending_perm is not None and msg.get("id") == pending_perm[0]:
            perm_id, prompt_id, _ = pending_perm
            pending_perm = None
            outcome = msg.get("result", {}).get("outcome", {})
            if cancel_requested:
                cancel_requested = False
                stop = "cancelled" if outcome.get("outcome") == "cancelled" else "refusal"
                send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": stop}})
                continue
            approved = outcome.get("optionId") == "allow"
            send({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "call_1",
                        "status": "completed" if approved else "failed",
                        "rawOutput": {
                            "output": "hello_acp\n" if approved else "denied",
                            "exit_code": 0 if approved else 1,
                        },
                    },
                },
            })
            reply_text = "DONE_APPROVED" if approved else "DONE_DENIED"
            send({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": reply_text},
                    },
                },
            })
            send({
                "jsonrpc": "2.0",
                "id": prompt_id,
                "result": {
                    "stopReason": "end_turn",
                    "usage": {
                        "inputTokens": 120,
                        "outputTokens": 15,
                        "cachedReadTokens": 40,
                        "cachedWriteTokens": 0,
                    },
                },
            })
            continue

        method = msg.get("method")
        req_id = msg.get("id")
        params = msg.get("params") or {}

        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": 1,
                    "agentInfo": {"name": "Antigravity ACP Server", "version": "1.5.15"},
                    "agentCapabilities": {
                        "loadSession": True,
                        "sessionCapabilities": {"resume": {}, "close": {}, "list": {}},
                    },
                },
            })
        elif method == "session/list":
            if unauth:
                send({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32000, "message": "Authentication required"},
                })
            else:
                send({"jsonrpc": "2.0", "id": req_id, "result": {"sessions": []}})
        elif method == "session/new":
            if unauth:
                send({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32000, "message": "Authentication required"},
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "sessionId": session_id,
                        "models": {"currentModelId": current_model},
                        "modes": dict(modes, currentModeId=current_mode),
                    },
                })
        elif method == "session/resume":
            requested = params.get("sessionId")
            if requested != session_id:
                send({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32602, "message": "Session not found"},
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "sessionId": session_id,
                        "models": {"currentModelId": current_model},
                        "modes": dict(modes, currentModeId=current_mode),
                    },
                })
        elif method == "session/set_model":
            model_id = params.get("modelId", "")
            if model_id == "invalid-model":
                send({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32602, "message": "Unknown model"},
                })
            else:
                current_model = model_id
                send({"jsonrpc": "2.0", "id": req_id, "result": {}})
        elif method == "session/set_mode":
            current_mode = params.get("modeId", "default")
            send({"jsonrpc": "2.0", "id": req_id, "result": {}})
        elif method == "session/prompt":
            prompt_text = params.get("prompt", [{}])[0].get("text", "")
            if prompt_text == "TRIGGER_PERMISSION":
                send({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call",
                            "toolCallId": "call_1",
                            "kind": "execute",
                            "status": "in_progress",
                            "rawInput": {"CommandLine": "echo hello_acp"},
                            "title": "echo hello_acp",
                        },
                    },
                })
                pending_perm = ("perm-1", req_id, prompt_text)
                send({
                    "jsonrpc": "2.0",
                    "id": "perm-1",
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session_id,
                        "toolCall": {
                            "toolCallId": "call_1",
                            "kind": "execute",
                            "rawInput": {"CommandLine": "echo hello_acp"},
                            "title": "echo hello_acp",
                        },
                        "options": [
                            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                            {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
                        ],
                    },
                })
            elif prompt_text == "TRIGGER_CANCEL":
                pending_perm = ("perm-cancel", req_id, prompt_text)
                send({
                    "jsonrpc": "2.0",
                    "id": "perm-cancel",
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session_id,
                        "toolCall": {
                            "toolCallId": "call_cancel",
                            "kind": "execute",
                            "rawInput": {"CommandLine": "sleep 60"},
                            "title": "sleep 60",
                        },
                        "options": [
                            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                            {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
                        ],
                    },
                })
            elif prompt_text == "TRIGGER_CRASH":
                send({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "partial"},
                        },
                    },
                })
                os._exit(3)
            elif prompt_text == "TRIGGER_BURST_EXIT":
                for _ in range(200):
                    send({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": "x"},
                            },
                        },
                    })
                send({"jsonrpc": "2.0", "id": req_id, "result": {"stopReason": "end_turn"}})
                os._exit(0)
            else:
                send({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_thought_chunk",
                            "content": {"type": "text", "text": "thinking..."},
                        },
                    },
                })
                send({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": f"Echo({current_model},{current_mode}): "},
                        },
                    },
                })
                send({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": prompt_text},
                        },
                    },
                })
                send({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "stopReason": "end_turn",
                        "usage": {
                            "inputTokens": 100,
                            "outputTokens": 20,
                            "cachedReadTokens": 10,
                            "cachedWriteTokens": 0,
                        },
                    },
                })
        elif method == "session/cancel":
            if pending_perm is not None:
                cancel_requested = True
        elif method == "session/close":
            if req_id is not None:
                send({"jsonrpc": "2.0", "id": req_id, "result": {}})
            return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
