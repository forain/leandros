#!/usr/bin/env python3
# Single persistent serial connection: runs a fixed test list with unique
# markers around each, dumping the whole stream to stdout. Avoids scmrun's
# per-call drain (which discarded mid-run output) and fixed-window lag.
import socket, time, select, sys

SOCK = "/tmp/leandros-serial.sock"
TESTS = [("vfstest", 70), ("drmsmoke", 18), ("scmtest", 30),
         ("epolltest", 18), ("evtest2", 18), ("idletest", 16)]

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(60):
    try:
        s.connect(SOCK); break
    except OSError:
        time.sleep(0.2)
else:
    print("CONNECT_FAIL"); sys.exit(1)
s.setblocking(False)

def answer(c):
    if b"\x1b[6n" in c:
        s.setblocking(True)
        s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
        s.setblocking(False)

def pump(maxdur, idle=5.0):
    end = time.time() + maxdur
    last = time.time()
    while time.time() < end:
        r, _, _ = select.select([s], [], [], 0.3)
        if r:
            try:
                c = s.recv(4096)
            except BlockingIOError:
                continue
            if not c:
                break
            sys.stdout.buffer.write(c); sys.stdout.flush()
            answer(c); last = time.time()
        elif time.time() - last > idle:
            break

def send(cmd):
    s.setblocking(True)
    p = (cmd + "\n").encode()
    for i in range(0, len(p), 8):
        s.sendall(p[i:i+8]); time.sleep(0.02)
    s.setblocking(False)

pump(3, idle=2.0)
for name, mx in TESTS:
    send("echo @@@BEGIN_%s@@@" % name); pump(5, idle=2.5)
    send(name); pump(mx, idle=6.0)
    send("echo @@@END_%s@@@" % name); pump(5, idle=2.5)
print("\n@@@ALL_TESTS_DONE@@@")
s.close()
