#!/usr/bin/env python3
"""HTTP server with Range support (Firefox seeks media with Range requests)."""
import http.server, os, re, sys

class H(http.server.SimpleHTTPRequestHandler):
    def send_head(self):
        path = self.translate_path(self.path.split('?')[0])
        if not os.path.isfile(path):
            return super().send_head()
        size = os.path.getsize(path)
        rng = self.headers.get('Range')
        f = open(path, 'rb')
        ctype = self.guess_type(path)
        if path.endswith('.webm'):
            ctype = 'video/webm'
        if rng:
            m = re.match(r'bytes=(\d*)-(\d*)', rng)
            a = int(m.group(1)) if m.group(1) else 0
            b = int(m.group(2)) if m.group(2) else size - 1
            b = min(b, size - 1)
            self.send_response(206)
            self.send_header('Content-Range', f'bytes {a}-{b}/{size}')
            self.send_header('Content-Length', str(b - a + 1))
            f.seek(a)
            self._left = b - a + 1
        else:
            self.send_response(200)
            self.send_header('Content-Length', str(size))
            self._left = size
        self.send_header('Content-Type', ctype)
        self.send_header('Accept-Ranges', 'bytes')
        self.end_headers()
        return f

    def copyfile(self, src, dst):
        left = getattr(self, '_left', None)
        while True:
            n = 65536 if left is None else min(65536, left)
            if n <= 0:
                break
            buf = src.read(n)
            if not buf:
                break
            try:
                dst.write(buf)
            except (BrokenPipeError, ConnectionResetError):
                break
            if left is not None:
                left -= len(buf)

    def log_message(self, fmt, *a):
        sys.stderr.write("%s %s\n" % (self.log_date_time_string(), fmt % a))

os.chdir(os.path.dirname(os.path.abspath(__file__)))
http.server.ThreadingHTTPServer(('0.0.0.0', int(sys.argv[1]) if len(sys.argv) > 1 else 8765), H).serve_forever()
