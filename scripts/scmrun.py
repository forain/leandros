#!/usr/bin/env python3
# Persistent serial reader: sends a command and reads for a fixed duration,
# ignoring the shell-prompt heuristic (scmtest's "-> " diagnostics trip the
# driver's early-break). Answers ESC[6n. Dumps raw serial to stdout.
#
# argv[3] is an OPTIONAL completion marker. Without it the reader always burns
# the whole duration, so a slow arch has to be budgeted for the worst case and
# every fast run pays that budget too. With it the read returns as soon as the
# marker appears; `dur` stays the hard ceiling, so a run that never prints the
# marker still ends, and still ends with everything it did print. Give a marker
# only the finished command emits -- an early break on a prefix truncates the
# log, and a truncated log greps clean.
import socket, sys, time, select

SOCK = "/tmp/leandros-serial.sock"
cmd = sys.argv[1] if len(sys.argv) > 1 else "scmtest"
dur = float(sys.argv[2]) if len(sys.argv) > 2 else 40.0
marker = sys.argv[3].encode() if len(sys.argv) > 3 and sys.argv[3] else None

# A marker that occurs in the command is not a completion marker. The tty
# echoes what it is sent, so the marker comes back as part of the echoed
# command line and the read ends before the command has printed anything --
# and what is left looks like a clean short run, not a truncated one, so it
# greps clean. artifacts/m13_suite.py sent `<test>; echo M13RC=$?` with marker
# "M13RC=" and every window therefore closed ~1 s after the command was typed:
# vfstest's 36 subtests arrived 16 in its own window and 20 in the next, every
# later row read the previous row's exit status, and four tests reported no
# exit status at all. Widening the budgets could not help, because nothing was
# timing out. Build the marker out of something the shell assembles instead:
#   <test>; echo "M13""RC=$?"      typed as M13""RC=, printed as M13RC=
if marker is not None and marker in (cmd + "\n").encode():
    sys.exit("scmrun: marker %r occurs in the command it is typed with, so the "
             "tty echo would end the read immediately. Emit it from something "
             "the shell assembles, e.g. echo \"M13\"\"RC=$?\"."
             % marker.decode(errors="replace"))

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
        if marker is not None and marker in buf:
            # Drain whatever is already in flight behind the marker, then stop.
            end = time.time() + 1.0
            while time.time() < end and select.select([s], [], [], 0.2)[0]:
                try:
                    tail = s.recv(4096)
                except BlockingIOError:
                    break
                if not tail:
                    break
                buf += tail
            break
s.close()
sys.stdout.buffer.write(buf)
sys.stdout.flush()
