#!/usr/bin/env python3
"""Host half of /bin/m15b-iced: is the stall libcosmic's single-instance probe?

m15-iced established the state: run standalone with its stderr preserved,
cosmic-settings emits four log records and then goes quiet, with WAYLAND_DEBUG=1
set the whole time and NOT ONE protocol line produced. It never opens a Wayland
connection, so it is not a rasteriser that draws nothing -- it never reaches a
renderer. The same boot's screendump showed wallpaper, panel bar, a ticking
clock and a raw wl_shm client window all composited correctly, which exonerates
the compositor without having to argue about it.

This run tests the mechanism. libcosmic's run_single_instance makes a BLOCKING
zbus call to its own APP_ID before building the application; busd 0.5.0 answers
a call to an unowned well-known name by dropping it with a warn! and never
replying, and has no reply timeout. run_single_instance honours
COSMIC_SINGLE_INSTANCE=false and skips D-Bus entirely, so the same binary in the
same session, one variable apart, is the whole experiment.

CTRL runs first ON PURPOSE: if the working arm ran first it would own the name
and CTRL's probe would then succeed and exit 0, which looks like a pass for
entirely the wrong reason.

usage: m15b_iced.py [outdir] [arch]
"""

import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
DRIVER = os.path.join(REPO, ".claude", "skills", "run-leandros", "driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"


# m12_caps.Serial is the reference implementation, but importing it drags in
# numpy + PIL for its VNC/venus capture half, which this run does not use and
# this host does not have. The class is small and its two non-obvious
# behaviours are load-bearing, so it is restated rather than depended on:
# QEMU's serial chardev serves ONE client at a time (driver.py must be finished
# with it before this connects), and the guest console asks for its cursor
# position with ESC[6n and blocks until answered.
import select
import socket

SERIAL_SOCK = "/tmp/leandros-serial.sock"


class Serial:
    def __init__(self, tee=None):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SERIAL_SOCK)
        self.s.setblocking(False)
        self.tee = open(tee, "ab", buffering=0) if tee else None
        self.buf = b""
        self.pump(0.5)

    def send(self, cmd):
        # Drop what is already buffered: a control satisfiable by text that
        # predates the command is not a control.
        self.buf = b""
        payload = (cmd + "\n").encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), 8):      # 16-byte PL011 RX FIFO
            self.s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        self.s.setblocking(False)

    def pump(self, secs):
        end = time.time() + secs
        while True:
            left = end - time.time()
            if left <= 0:
                return
            if select.select([self.s], [], [], min(0.2, left))[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return
                if b"\x1b[6n" in c:
                    self.s.setblocking(True)
                    self.s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                    self.s.setblocking(False)
                self.buf += c
                if self.tee:
                    self.tee.write(c)

    def read_until(self, pattern, timeout):
        end = time.time() + timeout
        while True:
            txt = self.buf.decode("utf-8", "replace")
            m = pattern.search(txt)
            if m:
                self.buf = txt[m.end():].encode("utf-8", "replace")
                return m, txt[:m.end()]
            if time.time() >= end:
                return None, txt
            self.pump(0.4)


MARK = re.compile(r"M15B: MARK (\w+) (\d+)|M15B: CAPTURES DONE")


def d(*args, t=120):
    return subprocess.run([sys.executable, DRIVER, *args],
                          capture_output=True, text=True, timeout=t)


def readppm(path):
    """P6 binary PPM -> (w, h, bytes). QEMU's screendump writes maxval 255."""
    with open(path, "rb") as f:
        raw = f.read()
    if not raw.startswith(b"P6"):
        return None
    fields, i = [], 2
    while len(fields) < 3:
        while i < len(raw) and raw[i:i + 1].isspace():
            i += 1
        if raw[i:i + 1] == b"#":
            while i < len(raw) and raw[i] != 0x0A:
                i += 1
            continue
        j = i
        while j < len(raw) and not raw[j:j + 1].isspace():
            j += 1
        fields.append(int(raw[i:j]))
        i = j
    return fields[0], fields[1], raw[i + 1:]


def census(px):
    """Cheap content summary that does not need numpy: how much of the frame is
    not the background colour, and how many distinct colours it holds."""
    n = len(px) // 3
    hist = {}
    for k in range(0, n * 3, 3 * 37):          # stride-sample; exactness is not the point
        hist[px[k:k + 3]] = hist.get(px[k:k + 3], 0) + 1
    top = sorted(hist.items(), key=lambda kv: -kv[1])[0]
    nonbg = sum(v for c, v in hist.items() if c != top[0])
    return len(hist), top[0].hex(), nonbg / max(1, sum(hist.values()))


def diffbox(a, b, w, h):
    """Bounding box of the pixels that differ, sampled every 4th row/col."""
    x0 = y0 = 1 << 30
    x1 = y1 = -1
    n = 0
    for y in range(0, h, 4):
        row = y * w * 3
        for x in range(0, w, 4):
            k = row + x * 3
            if a[k:k + 3] != b[k:k + 3]:
                n += 1
                x0, x1 = min(x0, x), max(x1, x)
                y0, y1 = min(y0, y), max(y1, y)
    if n == 0:
        return None
    return (x0, y0, x1, y1, n)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m15"
    arch = sys.argv[2] if len(sys.argv) > 2 else "aarch64"
    os.makedirs(out, exist_ok=True)

    if os.path.exists(SERIAL_LOG):
        os.unlink(SERIAL_LOG)
    print("=== boot ===", flush=True)
    r = d("start", arch, t=300)
    print(r.stdout[-1500:], r.stderr[-800:], flush=True)
    if "QEMU started" not in r.stdout:
        sys.exit("boot failed")
    r = d("login", "root", "root", t=90)
    print(r.stdout[-800:], flush=True)

    ser = Serial(tee=os.path.join(out, "serial.log"))

    print("\n=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    print(txt.strip()[-300:], flush=True)
    if not m:
        sys.exit(">>> CONTROL FAILED: absence and failure are indistinguishable "
                 "on this console. Aborting.")
    print(">>> CONTROL OK\n", flush=True)

    shots = []

    def shot(label):
        p = os.path.join(out, f"m15b-{arch}-{label}.ppm")
        d("screenshot", p, t=60)
        img = readppm(p) if os.path.exists(p) else None
        if img is None:
            print(f"  [shot {label}] NO CAPTURE", flush=True)
            return
        w, h, px = img
        ncol, bg, frac = census(px)
        prev = shots[-1] if shots else None
        db = diffbox(prev[3], px, w, h) if prev and len(prev[3]) == len(px) else None
        shots.append((label, w, h, px))
        print(f"  [shot {label}] {w}x{h} colours={ncol} bg=#{bg} "
              f"non-bg={frac:.3f} diff_vs_prev={db}", flush=True)

    ser.send("brush /bin/m15b-iced")

    while True:
        m, txt = ser.read_until(MARK, 900)
        print(txt.strip()[-4000:], flush=True)
        if not m:
            print(">>> TIMEOUT waiting for a MARK; the guest stopped early.", flush=True)
            break
        if m.group(0) == "M15B: CAPTURES DONE":
            print("\n>>> guest reports CAPTURES DONE", flush=True)
            break
        name = m.group(1)
        secs = int(m.group(2))
        print(f"\n===== PHASE {name} ({secs}s) =====", flush=True)
        t0 = time.time()
        # Sample each phase more than once. The first frame of the run is
        # suspect regardless (console text can be photographed), so the
        # comparison that matters is always between two later frames.
        sched = [4, secs * 0.45, secs - 6] if secs >= 20 else [secs - 3]
        for when in sched:
            left = when - (time.time() - t0)
            if left > 0:
                ser.pump(left)
            shot(f"{name.lower()}-t{int(time.time() - t0)}")
        left = secs - (time.time() - t0)
        if left > 0:
            ser.pump(left)

    print(">>> draining guest dump...", flush=True)
    _, txt = ser.read_until(re.compile(r"M15B: DONE"), 900)
    print(txt, flush=True)
    print(f">>> serial log: {os.path.join(out, 'serial.log')}", flush=True)


if __name__ == "__main__":
    main()
