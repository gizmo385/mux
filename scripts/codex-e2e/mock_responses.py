#!/usr/bin/env python3
"""Localhost mock of the OpenAI Responses API, enough to drive real `codex`.

Used by scripts/codex-e2e/smoke.sh so a live Codex end-to-end run costs no
API spend. Stdlib only.

Usage: mock_responses.py <port> <log> <modefile>

The modefile is re-read on every request: whitespace-separated key=value
pairs, e.g. `mode=tool delay=1`.

  mode=text   reply "mock reply" (a plain turn)
  mode=tool   first request of a turn: an escalated `exec_command`, which
              makes codex (approval_policy = "on-request") ask for approval;
              after the tool output comes back, "mock reply"
  mode=patch  first request of a turn: an `apply_patch` adding
              patched_by_mock.txt; then "mock reply"
  delay=N     sleep N seconds before answering (holds a turn open)

"First request of a turn" = no tool output since the last user message.
"""

import http.server
import itertools
import json
import sys
import time

LOG = sys.argv[2]
MODEF = sys.argv[3]
counter = itertools.count()

USAGE = {"input_tokens": 1, "input_tokens_details": None, "output_tokens": 1,
         "output_tokens_details": None, "total_tokens": 2}
PATCH = "*** Begin Patch\n*** Add File: patched_by_mock.txt\n+hello from mock\n*** End Patch\n"
TOOL_OUTPUTS = ("function_call_output", "custom_tool_call_output")


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


def read_mode():
    with open(MODEF) as f:
        cfg = dict(kv.split("=", 1) for kv in f.read().split() if "=" in kv)
    return cfg.get("mode", "text"), float(cfg.get("delay", "0"))


def tool_item(mode, body):
    if mode == "tool":
        return {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "exec_command",
                "arguments": json.dumps({
                    "cmd": "echo approved-by-smoke",
                    "sandbox_permissions": "require_escalated",
                    "justification": "smoke test: run a command outside the sandbox?"})}
    if mode == "patch":
        tools = [t for t in body.get("tools", []) if t.get("name") == "apply_patch"]
        if tools and tools[0].get("type") == "custom":
            return {"type": "custom_tool_call", "id": "ct_1", "call_id": "call_p",
                    "name": "apply_patch", "input": PATCH}
        if tools:
            return {"type": "function_call", "id": "fc_p", "call_id": "call_p",
                    "name": "apply_patch", "arguments": json.dumps({"input": PATCH})}
        return {"type": "function_call", "id": "fc_p", "call_id": "call_p", "name": "exec_command",
                "arguments": json.dumps({"cmd": "apply_patch <<'EOF'\n" + PATCH + "EOF\n"})}
    return None


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        i = next(counter)
        inp = body.get("input", [])
        last_user = max((n for n, x in enumerate(inp) if x.get("role") == "user"), default=-1)
        outputs = [x for x in inp[last_user + 1:] if x.get("type") in TOOL_OUTPUTS]
        mode, delay = read_mode()
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": time.time(), "i": i, "mode": mode, "n_outputs": len(outputs)}) + "\n")
        time.sleep(delay)
        item = tool_item(mode, body) if not outputs else None
        if item is None:
            item = {"type": "message", "role": "assistant", "id": f"msg_{i}",
                    "content": [{"type": "output_text", "text": "mock reply"}]}
        out = sse([
            {"type": "response.created", "response": {"id": f"resp_{i}"}},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": {"id": f"resp_{i}", "usage": USAGE}},
        ])
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
