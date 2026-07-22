#!/usr/bin/env python3
# Persistent serial reader: sends a command and reads for a fixed duration,
# ignoring the shell-prompt heuristic (scmtest's "-> " diagnostics trip the
# driver's early-break). Answers ESC[6n. Dumps raw serial to stdout.
import socket, sys, time, select

SOCK = "/tmp/leandros-serial.sock"
cmd = sys.argv[1] if len(sys.argv) > 1 else "scmtest"
dur = float(sys.argv[2]) if len(sys.argv) > 2 else 40.0

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(SOCK)
s.setblocking(False)
# drain
time.sleep(0.2)
try:
    while select.select([s], [], [], 0.1)[0]:
        s.recv(4096)
except Exception:
    pass
# send paced (16-byte PL011 RX FIFO)
payload = (cmd + "\n").encode()
s.setblocking(True)
for i in range(0, len(payload), 8):
    s.sendall(payload[i:i+8]); time.sleep(0.02)
s.setblocking(False)

buf = b""
deadline = time.time() + dur
while time.time() < deadline:
    if select.select([s], [], [], 0.2)[0]:
        try:
            chunk = s.recv(4096)
        except BlockingIOError:
            continue
        if not chunk:
            break
        buf += chunk
        if b"\x1b[6n" in chunk:
            s.setblocking(True)
            s.sendall(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
            s.setblocking(False)
s.close()
sys.stdout.buffer.write(buf)
sys.stdout.flush()
