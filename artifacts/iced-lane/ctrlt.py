#!/usr/bin/env python3
"""Send Ctrl-T (0x14) on the serial console and print the kernel task dump.
usage: LEANDROS_RUN_ID=iced python3 ctrlt.py [read_seconds]"""
import os, socket, select, sys, time
tag = os.environ.get("LEANDROS_RUN_ID", "")
sock = f"/tmp/leandros{'-' + tag if tag else ''}-serial.sock"
secs = float(sys.argv[1]) if len(sys.argv) > 1 else 4.0
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock)
s.setblocking(False)
s.sendall(b"\x14")
end = time.time() + secs
buf = b""
while time.time() < end:
    if select.select([s], [], [], 0.2)[0]:
        try:
            c = s.recv(65536)
        except BlockingIOError:
            continue
        if not c:
            break
        buf += c
s.close()
sys.stdout.write(buf.decode("utf-8", "replace"))
