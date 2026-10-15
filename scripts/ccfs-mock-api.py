#!/usr/bin/env python3
"""ccfs-mock-api.py [PORT] -- a scripted stand-in for the Anthropic Messages API, so the
Claude Code CLI inside LeandrOS can be driven through real tool calls with no account.

Point the guest at it with ANTHROPIC_BASE_URL=http://10.0.2.2:PORT (QEMU user networking's
host address) and any ANTHROPIC_API_KEY. Every request that offers tools gets the next step
of the script, chosen by how many tool results the conversation already carries:
  0: Bash   `echo bash-ran-$((6*7)); id -un`         (the task-output pin, an exec)
  1: Write  $CCFS_WRITE (default /home/leandro/ccfs/new/dir/file.txt), a new file in a
            directory that does not exist yet (mkdir through /proc/self/fd/N)
  2: Bash   `cat $CCFS_WRITE`
  3: text   "ccfs-script-done", end_turn
Requests without tools (titles, summaries) get a short text reply. Each request and the tool
results it carries are logged to stdout.
"""
import json, os, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
WPATH = os.environ.get("CCFS_WRITE", "/home/leandro/ccfs/new/dir/file.txt")
STEPS = [
    ("tool_use", "Bash", {"command": "echo bash-ran-$((6*7)); id -un", "description": "probe"}),
    ("tool_use", "Write", {"file_path": WPATH, "content": "written-by-claude-code\n"}),
    ("tool_use", "Bash", {"command": f"cat {WPATH}", "description": "read back"}),
    ("text", "ccfs-script-done", None),
]


def tool_results(body):
    out = []
    for m in body.get("messages", []):
        if isinstance(m.get("content"), list):
            for c in m["content"]:
                if c.get("type") == "tool_result":
                    out.append(c)
    return out


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send(self, code, ctype, data):
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        print("GET", self.path, flush=True)
        self._send(200, "application/json", b"{}")

    def do_POST(self):
        n = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(n)
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        path = self.path.split("?")[0]
        if not path.endswith("/v1/messages"):
            print("POST", self.path, flush=True)
            if path.endswith("count_tokens"):
                return self._send(200, "application/json", b'{"input_tokens": 10}')
            return self._send(200, "application/json", b"{}")
        model = body.get("model", "claude")
        if body.get("tools"):
            res = tool_results(body)
            for r in res[-1:]:
                print("  tool_result:", json.dumps(r)[:600], flush=True)
            kind, name, inp = STEPS[min(len(res), len(STEPS) - 1)]
        else:
            kind, name, inp = "text", "ok", None
        print(f"POST {self.path} tools={bool(body.get('tools'))} -> {kind} {name}", flush=True)
        if kind == "tool_use":
            block = {"type": "tool_use", "id": f"toolu_ccfs{len(tool_results(body))}", "name": name, "input": {}}
            stop = "tool_use"
        else:
            block = {"type": "text", "text": ""}
            stop = "end_turn"
        usage = {"input_tokens": 10, "output_tokens": 10}
        msg = {"id": "msg_ccfs", "type": "message", "role": "assistant", "model": model,
               "content": [], "stop_reason": None, "stop_sequence": None, "usage": usage}
        if not body.get("stream"):
            full = dict(block)
            if kind == "tool_use":
                full["input"] = inp
            else:
                full["text"] = name
            msg.update(content=[full], stop_reason=stop)
            return self._send(200, "application/json", json.dumps(msg).encode())
        ev = [("message_start", {"type": "message_start", "message": msg}),
              ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block})]
        if kind == "tool_use":
            ev.append(("content_block_delta", {"type": "content_block_delta", "index": 0,
                       "delta": {"type": "input_json_delta", "partial_json": json.dumps(inp)}}))
        else:
            ev.append(("content_block_delta", {"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": name}}))
        ev += [("content_block_stop", {"type": "content_block_stop", "index": 0}),
               ("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
                                  "usage": {"output_tokens": 10}}),
               ("message_stop", {"type": "message_stop"})]
        data = b"".join(f"event: {e}\ndata: {json.dumps(d)}\n\n".encode() for e, d in ev)
        self._send(200, "text/event-stream", data)


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), H).serve_forever()
