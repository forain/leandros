import http.server, os, sys, time, socketserver
OUT = sys.argv[1]; PORT = int(sys.argv[2])
CHUNK = os.urandom(1 << 20)
def log(s):
    with open(os.path.join(OUT, "server.log"), "a") as f: f.write(time.strftime("%H:%M:%S ") + s + "\n")
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        p = self.path.split("?")[0]
        if p.startswith("/blob/"):
            n = int(p.split("/")[2]); self.send_response(200)
            self.send_header("Content-Length", str(n)); self.end_headers()
            t0 = time.time(); sent = 0
            try:
                while sent < n:
                    k = min(len(CHUNK), n - sent); self.wfile.write(CHUNK[:k]); sent += k
                self.wfile.flush()
            except Exception as e: log(f"blob {self.path} from {self.client_address[0]} aborted after {sent} B: {e}")
            dt = time.time() - t0
            log(f"blob {self.path} from {self.client_address[0]} {sent} B {dt:.2f} s {sent*8/dt/1e6:.2f} Mbit/s (server-side)")
        else:
            self.send_response(200); self.end_headers(); self.wfile.write(b"ok\n"); log(f"GET {self.path} {self.client_address[0]}")
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0)); body = self.rfile.read(n)
        name = os.path.basename(self.path.split("?")[0]) or "post"
        with open(os.path.join(OUT, name + "-" + time.strftime("%H%M%S") + ".txt"), "wb") as f: f.write(body)
        log(f"POST {self.path} {n} B from {self.client_address[0]}")
        self.send_response(200); self.end_headers(); self.wfile.write(b"ok\n")
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): allow_reuse_address = True; daemon_threads = True
S(("0.0.0.0", PORT), H).serve_forever()
