#!/usr/bin/env python3
"""Scripted, offline stand-in for the Anthropic Messages API.

The main conversation is answered from a scenario file: the response for a request is the
scenario turn whose index equals the number of assistant messages already in the request, so
side requests never shift the script. Requests without tools get a short canned reply.

Each turn is {"delay": seconds before content, "pace": seconds between deltas,
"blocks": [{"type": "text", "text"} | {"type": "thinking", "thinking"} |
           {"type": "tool_use", "id", "name", "input"}], "stop_reason": optional}.

Usage: mock_anthropic.py SCENARIO.json PORT_FILE LOG.jsonl
"""

import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "claude-sonnet-4-5"
IDS = __import__("itertools").count(1)


def sse(handler, event, data):
    handler.wfile.write(f"event: {event}\ndata: {json.dumps(data)}\n\n".encode())
    handler.wfile.flush()


def chunks(text, size):
    return [text[i:i + size] for i in range(0, len(text), size)] or [""]


class Handler(BaseHTTPRequestHandler):
    scenario = None
    log = None
    lock = threading.Lock()

    def log_message(self, *_):
        pass

    def record(self, entry):
        with self.lock:
            with open(self.log, "a") as handle:
                handle.write(json.dumps(entry) + "\n")

    def do_GET(self):
        self.record({"method": "GET", "path": self.path})
        self.send_response(404)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"type":"error","error":{"type":"not_found_error","message":"mock"}}')

    def do_HEAD(self):
        self.send_response(200)
        self.end_headers()

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        path = self.path.split("?")[0]
        if path.endswith("/count_tokens"):
            return self.json({"input_tokens": 1200})
        if not path.endswith("/v1/messages"):
            self.record({"method": "POST", "path": self.path})
            return self.json({}, 404)
        messages = body.get("messages", [])
        tools = body.get("tools") or []
        assistant_turns = sum(1 for message in messages if message.get("role") == "assistant")
        turns = self.scenario["turns"]
        main = len(tools) > 0
        self.record({"path": self.path, "model": body.get("model"), "tools": len(tools),
                     "assistant_turns": assistant_turns, "stream": body.get("stream", False),
                     "max_tokens": body.get("max_tokens"), "main": main,
                     "tool_names": [tool.get("name") for tool in tools],
                     "shape": [(m.get("role"), [c.get("type") if isinstance(c, dict) else "str" for c in (m.get("content") if isinstance(m.get("content"), list) else ["str"])]) for m in messages]})
        if main and assistant_turns < len(turns):
            turn = turns[assistant_turns]
        else:
            turn = {"blocks": [{"type": "text", "text": self.scenario.get("side_reply", "Done")}]}
        if body.get("stream"):
            self.stream(turn, body.get("model", MODEL))
        else:
            self.json(self.message(turn, body.get("model", MODEL)))

    def json(self, payload, status=200):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    @staticmethod
    def stop_reason(turn):
        if turn.get("stop_reason"):
            return turn["stop_reason"]
        return "tool_use" if any(b["type"] == "tool_use" for b in turn["blocks"]) else "end_turn"

    def message(self, turn, model):
        content = []
        for block in turn["blocks"]:
            if block["type"] == "text":
                content.append({"type": "text", "text": block["text"]})
            elif block["type"] == "thinking":
                content.append({"type": "thinking", "thinking": block["thinking"], "signature": "mock"})
            elif block["type"] == "tool_use":
                content.append({"type": "tool_use", "id": block["id"], "name": block["name"],
                                "input": block["input"]})
        return {"id": f"msg_mock_{next(IDS)}", "type": "message", "role": "assistant", "model": model,
                "content": content, "stop_reason": self.stop_reason(turn), "stop_sequence": None,
                "usage": {"input_tokens": 1200, "output_tokens": 300}}

    def stream(self, turn, model):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        delay = turn.get("delay", 0.0)
        pace = turn.get("pace", 0.01)
        sse(self, "message_start", {"type": "message_start", "message": {
            "id": f"msg_mock_{next(IDS)}", "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 1200, "output_tokens": 1}}})
        if delay:
            time.sleep(delay)
        for index, block in enumerate(turn["blocks"]):
            kind = block["type"]
            if kind == "text":
                sse(self, "content_block_start", {"type": "content_block_start", "index": index,
                                                  "content_block": {"type": "text", "text": ""}})
                for piece in chunks(block["text"], 12):
                    sse(self, "content_block_delta", {"type": "content_block_delta", "index": index,
                                                      "delta": {"type": "text_delta", "text": piece}})
                    time.sleep(pace)
            elif kind == "thinking":
                sse(self, "content_block_start", {"type": "content_block_start", "index": index,
                                                  "content_block": {"type": "thinking", "thinking": "",
                                                                    "signature": ""}})
                for piece in chunks(block["thinking"], 12):
                    sse(self, "content_block_delta", {"type": "content_block_delta", "index": index,
                                                      "delta": {"type": "thinking_delta", "thinking": piece}})
                    time.sleep(pace)
                sse(self, "content_block_delta", {"type": "content_block_delta", "index": index,
                                                  "delta": {"type": "signature_delta", "signature": "mock"}})
            elif kind == "tool_use":
                sse(self, "content_block_start", {"type": "content_block_start", "index": index,
                                                  "content_block": {"type": "tool_use", "id": block["id"],
                                                                    "name": block["name"], "input": {}}})
                for piece in chunks(json.dumps(block["input"]), 24):
                    sse(self, "content_block_delta", {"type": "content_block_delta", "index": index,
                                                      "delta": {"type": "input_json_delta",
                                                                "partial_json": piece}})
                    time.sleep(pace)
            sse(self, "content_block_stop", {"type": "content_block_stop", "index": index})
        sse(self, "message_delta", {"type": "message_delta",
                                    "delta": {"stop_reason": self.stop_reason(turn), "stop_sequence": None},
                                    "usage": {"output_tokens": 300}})
        sse(self, "message_stop", {"type": "message_stop"})


def main():
    scenario_path, port_file, log_path = sys.argv[1:4]
    with open(scenario_path) as handle:
        Handler.scenario = json.load(handle)
    Handler.log = log_path
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(port_file, "w") as handle:
        handle.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
