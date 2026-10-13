#!/usr/bin/env python3
"""serve.py DIR OUTDIR [--port N] — hand self-host inputs to a guest, take its results back.

GET /<name>  serves DIR/<name> (flat: no subdirectories, no listing)
PUT /<name>  writes the body to OUTDIR/<name> (basename only, atomically)

The guest reaches the host at its default gateway: 10.0.2.2 under SLIRP,
192.168.105.1 under socket_vmnet. Binds 0.0.0.0 so both work; nothing here is
meant for a shared network. See docs/development/selfhost.md.
"""
import argparse
import http.server
import os
import sys


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("outdir")
    ap.add_argument("--port", type=int, default=8765)
    a = ap.parse_args()
    src = os.path.realpath(a.dir)
    out = os.path.realpath(a.outdir)
    os.makedirs(out, exist_ok=True)

    class H(http.server.BaseHTTPRequestHandler):
        def _name(self):
            n = os.path.basename(self.path.split("?", 1)[0])
            if not n or n.startswith("."):
                self.send_error(404)
                return None
            return n

        def do_GET(self):
            n = self._name()
            if n is None:
                return
            p = os.path.join(src, n)
            if not os.path.isfile(p):
                self.send_error(404)
                return
            size = os.path.getsize(p)
            self.send_response(200)
            self.send_header("Content-Length", str(size))
            self.send_header("Content-Type", "application/octet-stream")
            self.end_headers()
            with open(p, "rb") as f:
                while True:
                    b = f.read(1 << 20)
                    if not b:
                        break
                    self.wfile.write(b)

        def do_PUT(self):
            n = self._name()
            if n is None:
                return
            left = int(self.headers.get("Content-Length", "0"))
            tmp = os.path.join(out, "." + n + ".part")
            with open(tmp, "wb") as f:
                while left > 0:
                    b = self.rfile.read(min(left, 1 << 20))
                    if not b:
                        break
                    f.write(b)
                    left -= len(b)
            if left:
                os.unlink(tmp)
                self.send_error(400, "short body")
                return
            os.replace(tmp, os.path.join(out, n))
            self.send_response(201)
            self.send_header("Content-Length", "0")
            self.end_headers()
            print(f"received {n} ({os.path.getsize(os.path.join(out, n))} bytes)", file=sys.stderr, flush=True)

    s = http.server.ThreadingHTTPServer(("0.0.0.0", a.port), H)
    print(f"serving {src} on :{a.port}, uploads -> {out}", file=sys.stderr, flush=True)
    s.serve_forever()


if __name__ == "__main__":
    main()
