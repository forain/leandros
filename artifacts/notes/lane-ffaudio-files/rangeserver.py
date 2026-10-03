import http.server, os, re, sys
class H(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def send_head(self):
        path = self.translate_path(self.path.split('?')[0])
        if os.path.isdir(path) or not os.path.exists(path):
            return super().send_head()
        size = os.path.getsize(path); f = open(path, 'rb')
        rng = self.headers.get('Range'); start, end = 0, size - 1
        m = re.match(r'bytes=(\d*)-(\d*)', rng or '')
        if m:
            if m.group(1): start = int(m.group(1))
            if m.group(2): end = min(int(m.group(2)), size - 1)
            self.send_response(206); self.send_header('Content-Range', f'bytes {start}-{end}/{size}')
        else:
            self.send_response(200)
        ct = self.guess_type(path)
        self.send_header('Content-Type', ct); self.send_header('Accept-Ranges', 'bytes')
        self.send_header('Content-Length', str(end - start + 1)); self.send_header('Cache-Control', 'no-store')
        self.end_headers(); f.seek(start); self.range_left = end - start + 1
        return f
    def copyfile(self, src, dst):
        left = getattr(self, 'range_left', None)
        if left is None: return super().copyfile(src, dst)
        while left > 0:
            b = src.read(min(65536, left))
            if not b: break
            dst.write(b); left -= len(b)
http.server.ThreadingHTTPServer(('0.0.0.0', int(sys.argv[1])), H).serve_forever()
