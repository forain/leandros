#!/usr/bin/env python3
"""fake-imds.py STATE_DIR -- a fake EC2 instance metadata service (IMDSv2) for
the ec2init test. Speaks HTTP/1.1 on stdin/stdout, so QEMU's slirp can run it
per connection:

    -netdev user,id=net0,guestfwd=tcp:10.0.2.100:80-cmd:python3 -I scripts/fake-imds.py /tmp/imds

STATE_DIR/config.json (re-read per request, the test edits it between runs):
    {"instance_id": "i-0123", "key": "ssh-ed25519 AAAA... comment",
     "hostname": "ip-10-0-0-5.ec2.internal", "user_data": ""}
STATE_DIR/tokens   issued tokens, one per line (so state survives the
                   one-process-per-connection model)
STATE_DIR/log      one line per request: "METHOD path auth=ok|none|bad -> status"

IMDSv2 semantics enforced: PUT /latest/api/token needs a
X-aws-ec2-metadata-token-ttl-seconds header in 1..21600 (else 400) and returns
a token; every GET needs a valid X-aws-ec2-metadata-token (else 401).
"""
import json
import os
import secrets
import sys

STATE = sys.argv[1] if len(sys.argv) > 1 else "."
TTL = "x-aws-ec2-metadata-token-ttl-seconds"
TOK = "x-aws-ec2-metadata-token"
REASON = {200: "OK", 400: "Bad Request", 401: "Unauthorized", 404: "Not Found", 405: "Method Not Allowed"}


def config():
    try:
        with open(os.path.join(STATE, "config.json")) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def log(line):
    try:
        with open(os.path.join(STATE, "log"), "a") as f:
            f.write(line + "\n")
    except OSError:
        pass


def valid(tok):
    try:
        with open(os.path.join(STATE, "tokens")) as f:
            return tok in f.read().split()
    except OSError:
        return False


def route(method, path, hdrs):
    """-> (status, body, auth_state)"""
    if path == "/latest/api/token":
        if method != "PUT":
            return 405, "", "none"
        try:
            ttl = int(hdrs.get(TTL, ""))
        except ValueError:
            return 400, "", "none"
        if not 1 <= ttl <= 21600:
            return 400, "", "none"
        tok = secrets.token_urlsafe(40)
        with open(os.path.join(STATE, "tokens"), "a") as f:
            f.write(tok + "\n")
        return 200, tok, "none"
    t = hdrs.get(TOK)
    auth = "none" if t is None else ("ok" if valid(t) else "bad")
    if method != "GET":
        return 405, "", auth
    if auth != "ok":
        return 401, "", auth
    c = config()
    pre = "/latest/meta-data/"
    if path == "/latest/user-data":
        return (200, c["user_data"], auth) if c.get("user_data") else (404, "", auth)
    if not path.startswith(pre):
        return 404, "", auth
    leaf = path[len(pre):]
    table = {"instance-id": c.get("instance_id"), "local-hostname": c.get("hostname"),
             "public-keys/": "0=fake-key" if c.get("key") else None,
             "public-keys/0/openssh-key": c.get("key")}
    v = table.get(leaf)
    return (200, v + ("\n" if leaf.endswith("/") else ""), auth) if v else (404, "", auth)


def main():
    rd, wr = sys.stdin.buffer, sys.stdout.buffer
    while True:
        line = rd.readline()
        if not line:
            return
        parts = line.decode("latin-1").split()
        if len(parts) < 3:
            return
        method, path = parts[0], parts[1]
        hdrs = {}
        while True:
            h = rd.readline().decode("latin-1").strip()
            if not h:
                break
            k, _, v = h.partition(":")
            hdrs[k.strip().lower()] = v.strip()
        n = int(hdrs.get("content-length", "0") or 0)
        if n:
            rd.read(n)
        status, body, auth = route(method, path, hdrs)
        log(f"{method} {path} auth={auth} -> {status}")
        data = body.encode()
        wr.write((f"HTTP/1.1 {status} {REASON.get(status, 'Error')}\r\nContent-Length: {len(data)}\r\n"
                  f"Content-Type: text/plain\r\nServer: EC2ws\r\n\r\n").encode() + data)
        wr.flush()
        if hdrs.get("connection", "").lower() == "close":
            return


if __name__ == "__main__":
    main()
