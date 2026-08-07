#!/usr/bin/env python3
"""Empirical capability probe of the COSMIC desktop: what a user can actually do.

Host half of a pair. The guest half is /bin/m12-caps (artifacts/m6-session-data/
m12-caps), which brings the session up, opens clients, and announces timed
windows as `M12: MARK <NAME> <SECS>`. This script listens for those, injects
input over QMP, and photographs the scanout over RFB.

CAPTURE ROUTE — settled, not re-litigated here. `-display egl-headless` is a
GL->CPU converter whose egl_scanout_flush() blits into the 2D console surface;
a paired `-vnc ...,display=venusgpu` listener is the 2D consumer that reads it.
`screendump` is structurally unable to photograph this at any `device=`
(qemu_console_surface() returns NULL for SCANOUT_TEXTURE). See m9_vkcap.py.

INPUT ROUTE. QMP `input-send-event` into virtio-tablet-pci (absolute pointer)
and virtio-keyboard-pci. QMP accepting the command proves only that the HOST
queued it, which is why the kernel's own evdev counter matters: `evpush` in the
[DRMSTAT] line is the guest-side witness that the event reached the ring. With
it, "the compositor ignored the pointer" and "the pointer never arrived" are
different findings; without it they are the same silence. [DRMSTAT] requires
drivers/src/drm_device_interface.rs:1734 `DRM_STATS = true` — the harness says
so rather than reporting zeros if the kernel was built with it off.

WHY EVERY NULL RESULT NEEDS THE CLOCK. The panel clock repaints once a second.
So any two captures seconds apart must differ somewhere even when the thing
under test did nothing, and a capture that is byte-identical to the one before
it is a STALE FRAME, not a quiet desktop. The IDLE phase measures that region
first; afterwards, "diff confined to the liveness region" is how this script
says "the compositor is alive and your keypress did nothing", and
"diff empty" is how it says "do not trust this capture at all".

Serial and VNC and QMP are three separate channels. HMP/monitor is never opened.

usage: m12_caps.py [outdir] [arch]
"""

import collections
import json
import math
import os
import re
import select
import socket
import struct
import sys
import time

import numpy as np
from PIL import Image

SERIAL_SOCK = "/tmp/leandros-serial.sock"
QMP_SOCK = "/tmp/leandros-qmp.sock"
VNC_HOST, VNC_PORT = "127.0.0.1", 5909


# ---------------------------------------------------------------- serial ----
class Serial:
    """Held open for the whole run. QEMU's serial chardev serves ONE client at
    a time, so driver.py must be finished with it before this connects."""

    def __init__(self, tee=None):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SERIAL_SOCK)
        self.s.setblocking(False)
        self.tee_path = tee
        self.tee = open(tee, "ab", buffering=0) if tee else None
        self.buf = b""
        self.drain(0.5)

    def _stash(self, chunk):
        self.buf += chunk
        if self.tee:
            self.tee.write(chunk)

    def drain(self, secs):
        end = time.time() + secs
        while time.time() < end:
            if select.select([self.s], [], [], 0.1)[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return
                self._stash(c)

    def send(self, cmd):
        # Drop what is already buffered: boot chatter contains phrases a later
        # sentinel regex would match, and a control satisfiable by text that
        # predates the command is not a control.
        self.buf = b""
        payload = (cmd + "\n").encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), 8):
            self.s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        self.s.setblocking(False)

    def pump(self, secs):
        """Read for `secs` without consuming the buffer. Used while we are busy
        injecting input, so a MARK that lands meanwhile is still found later."""
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
                self._stash(c)

    def read_until(self, pattern, timeout):
        end = time.time() + timeout
        while True:
            m = pattern.search(self.buf.decode("utf-8", "replace"))
            if m:
                txt = self.buf.decode("utf-8", "replace")
                self.buf = txt[m.end():].encode("utf-8", "replace")
                return m, txt[:m.end()]
            if time.time() >= end:
                return None, self.buf.decode("utf-8", "replace")
            self.pump(0.4)

    # ---- [DRMSTAT] ----
    # Field names, never positions: c5abb8d once inserted five dmg_* fields
    # mid-line and silently zeroed every position-keyed parser downstream.
    KV = re.compile(r"([a-z_]+)=0x([0-9A-Fa-f]+)")

    def last_drmstat(self):
        if not self.tee_path:
            return None
        try:
            with open(self.tee_path, "rb") as f:
                data = f.read()
        except OSError:
            return None
        last = None
        for line in data.decode("utf-8", "replace").splitlines():
            i = line.find("[DRMSTAT]")
            if i < 0:
                continue
            rec = {k: int(v, 16) for k, v in self.KV.findall(line[i:])}
            if "t" in rec:
                last = rec
        return last


# ------------------------------------------------------------------- qmp ----
class Qmp:
    """Persistent connection: reconnecting per event caps the rate far below the
    ~60/s needed to reproduce a real pointer burst."""

    def __init__(self, w=1920, h=1080):
        self.f = None
        self.w, self.h = w, h
        self.sent = 0
        self.rejected = 0
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(5)
            s.connect(QMP_SOCK)
            self.s = s
            self.f = s.makefile("rwb")
            self.f.readline()
            self.f.write(b'{"execute":"qmp_capabilities"}\n')
            self.f.flush()
            self.f.readline()
        except Exception as e:
            print(f"[qmp] connect failed: {e}", flush=True)
            self.f = None

    def _send(self, events):
        if not self.f:
            return False
        ev = {"execute": "input-send-event", "arguments": {"events": events}}
        try:
            self.f.write((json.dumps(ev) + "\n").encode())
            self.f.flush()
            resp = self.f.readline().decode(errors="replace")
            self.sent += 1
            if "return" not in resp:
                self.rejected += 1
                if self.rejected <= 3:
                    print(f"[qmp] REJECTED {json.dumps(events)[:120]} -> "
                          f"{resp.strip()[:200]}", flush=True)
                return False
            return True
        except Exception as e:
            print(f"[qmp] send failed: {e}", flush=True)
            self.f = None
            return False

    def move(self, x, y):
        # One event per axis, as the M8 cursor harness did. The combined
        # two-axis form is not what was measured to work, and a rejected
        # command looks exactly like "the compositor ignored the pointer".
        ok = True
        for axis, val, span in (("x", x, self.w), ("y", y, self.h)):
            v = max(0, min(span - 1, int(val)))
            ok &= self._send([{"type": "abs", "data": {
                "axis": axis, "value": int(v * 0x7FFF / span)}}])
        return ok

    def btn(self, button, down):
        return self._send([{"type": "btn",
                            "data": {"down": bool(down), "button": button}}])

    def click(self, x, y, button="left"):
        self.move(x, y)
        time.sleep(0.25)
        self.btn(button, True)
        time.sleep(0.12)
        self.btn(button, False)

    def key(self, qcode, down):
        return self._send([{"type": "key", "data": {
            "key": {"type": "qcode", "data": qcode}, "down": bool(down)}}])

    def tap(self, qcode, mods=()):
        for m in mods:
            self.key(m, True)
            time.sleep(0.05)
        self.key(qcode, True)
        time.sleep(0.08)
        self.key(qcode, False)
        for m in reversed(mods):
            time.sleep(0.05)
            self.key(m, False)

    def typestr(self, s):
        for ch in s:
            self.tap(ch)
            time.sleep(0.12)

    def drag(self, x0, y0, x1, y1, button="left", mods=(), steps=24):
        self.move(x0, y0)
        time.sleep(0.3)
        for m in mods:
            self.key(m, True)
            time.sleep(0.06)
        self.btn(button, True)
        time.sleep(0.15)
        for i in range(1, steps + 1):
            self.move(x0 + (x1 - x0) * i / steps, y0 + (y1 - y0) * i / steps)
            time.sleep(0.03)
        time.sleep(0.2)
        self.btn(button, False)
        for m in reversed(mods):
            time.sleep(0.06)
            self.key(m, False)

    def sweep(self, secs, rate=60):
        """Lissajous so every move is a genuinely new position."""
        t0 = time.time()
        n = 0
        while time.time() - t0 < secs:
            p = time.time() - t0
            x = self.w * 0.5 + self.w * 0.35 * math.sin(p * 1.7)
            y = self.h * 0.5 + self.h * 0.30 * math.sin(p * 2.3)
            if self.move(x, y):
                n += 1
            time.sleep(1.0 / rate)
        return n, time.time() - t0


# ------------------------------------------------------------------- rfb ----
def recvall(s, n):
    out = b""
    while len(out) < n:
        chunk = s.recv(n - len(out))
        if not chunk:
            raise EOFError(f"VNC closed with {len(out)}/{n} bytes")
        out += chunk
    return out


def vnc_capture(timeout=60.0):
    s = socket.create_connection((VNC_HOST, VNC_PORT), timeout=10)
    s.settimeout(15)
    ver = recvall(s, 12)
    if not ver.startswith(b"RFB "):
        raise RuntimeError(f"not an RFB server: {ver!r}")
    s.sendall(b"RFB 003.008\n")
    ntypes = recvall(s, 1)[0]
    if ntypes == 0:
        rlen = struct.unpack(">I", recvall(s, 4))[0]
        raise RuntimeError(f"VNC handshake refused: {recvall(s, rlen)!r}")
    types = recvall(s, ntypes)
    if 1 not in types:
        raise RuntimeError(f"VNC needs auth; offered {list(types)}")
    s.sendall(bytes([1]))
    if struct.unpack(">I", recvall(s, 4))[0] != 0:
        rlen = struct.unpack(">I", recvall(s, 4))[0]
        raise RuntimeError(f"VNC auth failed: {recvall(s, rlen)!r}")
    s.sendall(bytes([1]))  # ClientInit, shared
    w, h = struct.unpack(">HH", recvall(s, 4))
    recvall(s, 16)
    nlen = struct.unpack(">I", recvall(s, 4))[0]
    recvall(s, nlen)

    # 32bpp true colour LE, R=16 G=8 B=0, so '<I' reads 0x00RRGGBB.
    pixfmt = struct.pack(">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
    s.sendall(b"\x00\x00\x00\x00" + pixfmt)
    s.sendall(b"\x02\x00" + struct.pack(">H", 1) + struct.pack(">i", 0))  # Raw

    fb = bytearray(w * h * 4)
    seen = np.zeros(w * h, dtype=bool)
    end = time.time() + timeout
    rects = 0
    while time.time() < end:
        s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))  # non-incremental
        got = False
        inner = time.time() + 20
        while time.time() < inner:
            s.settimeout(max(0.5, inner - time.time()))
            try:
                mtype = recvall(s, 1)[0]
            except socket.timeout:
                break
            if mtype == 0:
                recvall(s, 1)
                nrects = struct.unpack(">H", recvall(s, 2))[0]
                for _ in range(nrects):
                    rx, ry, rw, rh, enc = struct.unpack(">HHHHi", recvall(s, 12))
                    if enc != 0:
                        raise RuntimeError(f"unexpected encoding {enc}")
                    data = recvall(s, rw * rh * 4)
                    for row in range(rh):
                        src = row * rw * 4
                        dst = ((ry + row) * w + rx) * 4
                        fb[dst:dst + rw * 4] = data[src:src + rw * 4]
                        seen[(ry + row) * w + rx:(ry + row) * w + rx + rw] = True
                    rects += 1
                got = True
                break
            elif mtype == 1:
                recvall(s, 3)
                n = struct.unpack(">H", recvall(s, 2))[0]
                recvall(s, n * 6)
            elif mtype == 2:
                pass
            elif mtype == 3:
                recvall(s, 3)
                n = struct.unpack(">I", recvall(s, 4))[0]
                recvall(s, n)
            else:
                raise RuntimeError(f"unknown RFB server message {mtype}")
        if seen.all():
            break
        if not got:
            time.sleep(0.3)
    s.close()
    return w, h, bytes(fb), int((~seen).sum()), rects


# -------------------------------------------------------------- analysis ----
class Frame:
    def __init__(self, name, w, h, fb, missing, rects, t):
        self.name, self.w, self.h = name, w, h
        self.missing, self.rects, self.t = missing, rects, t
        a = np.frombuffer(fb, dtype=np.uint8).reshape(h, w, 4)
        self.rgb = a[:, :, [2, 1, 0]].copy()          # B,G,R,X -> R,G,B
        self.packed = (a[:, :, 2].astype(np.uint32) << 16 |
                       a[:, :, 1].astype(np.uint32) << 8 |
                       a[:, :, 0].astype(np.uint32))

    def png(self, path):
        Image.fromarray(self.rgb, "RGB").save(path)

    def census(self, top=6):
        vals, cnts = np.unique(self.packed, return_counts=True)
        order = np.argsort(-cnts)[:top]
        return len(vals), [(int(vals[i]), int(cnts[i])) for i in order]


def diff(a, b):
    """Changed-pixel count and bounding box between two frames."""
    if a is None or b is None or a.packed.shape != b.packed.shape:
        return None
    m = a.packed != b.packed
    n = int(m.sum())
    if n == 0:
        return dict(n=0, box=None, frac=0.0)
    ys, xs = np.nonzero(m)
    return dict(n=n, frac=n / m.size,
                box=(int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())))


def boxstr(d):
    if d is None:
        return "n/a"
    if d["n"] == 0:
        return "IDENTICAL (0 px changed)"
    x0, y0, x1, y1 = d["box"]
    return (f"{d['n']} px ({100 * d['frac']:.3f}%) bbox x={x0}..{x1} "
            f"y={y0}..{y1} ({x1 - x0 + 1}x{y1 - y0 + 1})")


def changed_box(a, b, exclude=None, min_px=400):
    """Bounding box of what changed between two frames, ignoring `exclude`.

    `exclude` is the liveness region — the panel clock, which repaints once a
    second and therefore differs between ANY two captures. Left in, it drags
    every bounding box up to the panel and turns "where is the new window" into
    "somewhere between the clock and the window", which is not a coordinate you
    can click. Returned box is None when too little changed to be a real
    object, so a caller can say it found nothing instead of clicking noise."""
    if a is None or b is None or a.packed.shape != b.packed.shape:
        return None, 0
    m = a.packed != b.packed
    if exclude:
        x0, y0, x1, y1 = exclude
        m[y0:y1 + 1, x0:x1 + 1] = False
    n = int(m.sum())
    if n < min_px:
        return None, n
    ys, xs = np.nonzero(m)
    return (int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())), n


def centre(box):
    x0, y0, x1, y1 = box
    return (x0 + x1) // 2, (y0 + y1) // 2


# ----------------------------------------------------------------- state ----
FRAMES = {}
ORDER = []
OUT = "/tmp/m12"
STATS = {}


def cap(name, log=True):
    t0 = time.time()
    w, h, fb, missing, rects = vnc_capture()
    f = Frame(name, w, h, fb, missing, rects, time.time())
    FRAMES[name] = f
    ORDER.append(name)
    f.png(os.path.join(OUT, f"{name}.png"))
    if log:
        nd, top = f.census()
        print(f"  [cap {name}] {w}x{h} uncovered={missing} rects={rects} "
              f"colours={nd} top=0x{top[0][0]:06x}x{top[0][1]} "
              f"({time.time() - t0:.1f}s)", flush=True)
    return f


def snap(ser, label):
    r = ser.last_drmstat()
    STATS[label] = r
    return r


def drm_delta(a, b, keys=("evpush", "flips_sub", "curs_mv", "curs_up",
                          "atomic", "dirtyfb", "dmg_rect", "dmg_full")):
    ra, rb = STATS.get(a), STATS.get(b)
    if not ra or not rb:
        return "[DRMSTAT] unavailable (kernel built with DRM_STATS = false?)"
    dt = (rb.get("t", 0) - ra.get("t", 0)) / 100.0
    if dt <= 0:
        return f"[DRMSTAT] no tick advance between {a} and {b}"
    parts = [f"dt={dt:.1f}s"]
    for k in keys:
        parts.append(f"{k}+{rb.get(k, 0) - ra.get(k, 0)}")
    return "  ".join(parts)


# ---------------------------------------------------------------- phases ----
# Each entry is a list of (offset_seconds_into_the_window, callable). Offsets
# must leave room for a capture (~2-6 s at 1920x1080) — a capture that lands
# after its window closed photographs the NEXT phase, which is exactly how a
# stale frame becomes a confident wrong answer.
# Discovered geometry. Every coordinate this harness clicks is MEASURED from
# the frames, never assumed: the panel's edge, the window manager's placement
# policy and the window size are all things this run is supposed to find out,
# so hardcoding them would let a miss ("I clicked empty desktop") masquerade as
# a negative ("clicking the window did nothing").
LIVE_BOX = None      # the clock: the region that changes with nothing provoked
TARGET = {}          # name -> (x0, y0, x1, y1) of a discovered object


def learn_liveness():
    a, b, c = (FRAMES.get("I1_idle_a"), FRAMES.get("I2_idle_b"),
               FRAMES.get("I3_idle_c"))
    global LIVE_BOX
    if not (a and b and c):
        return
    boxes = [d["box"] for d in (diff(a, b), diff(b, c)) if d and d["n"]]
    if not boxes:
        print("  [learn] nothing changed across the idle captures — there is "
              "no liveness region, so no later null result is interpretable.",
              flush=True)
        return
    LIVE_BOX = (min(b_[0] for b_ in boxes), min(b_[1] for b_ in boxes),
                max(b_[2] for b_ in boxes), max(b_[3] for b_ in boxes))
    x0, y0, x1, y1 = LIVE_BOX
    print(f"  [learn] liveness region x={x0}..{x1} y={y0}..{y1} "
          f"({x1 - x0 + 1}x{y1 - y0 + 1}) — the self-repainting part of the "
          f"desktop, excluded from every object search below.", flush=True)


def learn(tag, ref_name, cap_name):
    """Find whatever appeared between two captures and remember where."""
    box, n = changed_box(FRAMES.get(ref_name), FRAMES.get(cap_name), LIVE_BOX)
    if box is None:
        print(f"  [learn] {tag}: nothing appeared vs {ref_name} "
              f"({n} px changed outside the liveness region)", flush=True)
        return None
    TARGET[tag] = box
    x0, y0, x1, y1 = box
    print(f"  [learn] {tag}: x={x0}..{x1} y={y0}..{y1} "
          f"({x1 - x0 + 1}x{y1 - y0 + 1}, {n} px)", flush=True)
    return box


def aim(tag, fallback):
    """Centre of a discovered object, or the fallback — saying which."""
    box = TARGET.get(tag)
    if box is None:
        print(f"  [aim] {tag} was never located; falling back to {fallback}",
              flush=True)
        return fallback
    return centre(box)


def phase_actions(name, q, W, H):
    P = []

    def at(t, fn):
        P.append((t, fn))

    if name == "IDLE":
        # Three spaced captures with NOTHING provoked. I1-vs-I2 and I2-vs-I3
        # establish the liveness region (the clock) and prove the capture is
        # not stale before any null result is claimed anywhere else.
        at(4, lambda: cap("I1_idle_a"))
        at(20, lambda: cap("I2_idle_b"))
        at(38, lambda: cap("I3_idle_c"))
        at(44, learn_liveness)

    elif name == "POINTER":
        at(2, lambda: q.sweep(18))
        at(24, lambda: q.move(200, 200))
        at(28, lambda: cap("P1_ptr_tl"))
        at(38, lambda: q.move(W - 200, H - 200))
        at(42, lambda: cap("P2_ptr_br"))
        at(52, lambda: q.sweep(16))
        at(72, lambda: q.move(W // 2, H // 2))
        at(76, lambda: cap("P3_ptr_mid"))
        at(86, lambda: cap("P4_ptr_mid_again"))

    elif name == "CLICK":
        # The clock IS in the panel, so the liveness region locates the bar
        # without assuming which edge it is docked to. Click the applet, then
        # the far end of the same bar row, then the desktop.
        at(2, lambda: q.click(*aim("clock", (W // 2, 16))))
        at(8, lambda: cap("C1_panel_applet_click"))
        at(20, lambda: q.click(120, aim("clock", (W // 2, 16))[1]))
        at(26, lambda: cap("C2_panel_far_click"))
        at(36, lambda: q.click(W // 2, H // 2))
        at(42, lambda: cap("C3_desktop_click"))

    elif name in ("KEY_SUPER", "KEY_SLASH", "KEY_A"):
        combo = {"KEY_SUPER": ("meta_l", ()),
                 "KEY_SLASH": ("slash", ("meta_l",)),
                 "KEY_A": ("a", ("meta_l",))}[name]
        tag = name.lower()
        at(2, lambda: cap(f"K_{tag}_before"))
        at(10, lambda: q.tap(combo[0], combo[1]))
        at(16, lambda: cap(f"K_{tag}_after1"))
        at(28, lambda: cap(f"K_{tag}_after2"))
        at(30, lambda: learn(tag, f"K_{tag}_before", f"K_{tag}_after1"))
        at(38, lambda: q.tap("esc"))
        at(44, lambda: cap(f"K_{tag}_esc"))

    elif name == "WIN1":
        at(4, lambda: cap("W1_one_window"))
        at(8, lambda: learn("win1", "I3_idle_c", "W1_one_window"))
        at(14, lambda: q.click(*aim("win1", (W // 2, H // 2))))
        at(20, lambda: cap("W2_after_click"))
        at(30, lambda: q.typestr("abc"))
        at(36, lambda: cap("W3_after_keys"))
        at(46, lambda: q.typestr("def"))
        at(52, lambda: cap("W4_after_more_keys"))
        at(62, lambda: cap("W5_settle"))

    elif name == "WIN2":
        at(4, lambda: cap("X1_two_windows"))
        at(8, lambda: learn("win2", "W5_settle", "X1_two_windows"))
        at(14, lambda: q.click(*aim("win1", (W // 2 - 300, H // 2))))
        at(20, lambda: cap("X2_click_win1"))
        at(30, lambda: q.click(*aim("win2", (W // 2 + 300, H // 2))))
        at(36, lambda: cap("X3_click_win2"))
        at(46, lambda: q.typestr("xyz"))
        at(52, lambda: cap("X4_keys_to_focused"))
        at(62, lambda: cap("X5_settle"))

    elif name == "WM":
        # Drag the window that was clicked last (win2, so it should be the
        # focused one) by its own centre, not by a guessed screen coordinate.
        at(2, lambda: q.drag(*aim("win2", (W // 2, H // 2)),
                             W // 4, H // 4, "left", ("meta_l",)))
        at(16, lambda: cap("M1_after_move"))
        at(28, lambda: q.drag(W // 4, H // 4, W // 4 + 300, H // 4 + 250,
                              "right", ("meta_l",)))
        at(42, lambda: cap("M2_after_resize"))
        at(54, lambda: q.tap("q", ("meta_l",)))
        at(60, lambda: cap("M3_after_close1"))
        at(72, lambda: q.tap("q", ("meta_l",)))
        at(78, lambda: cap("M4_after_close2"))
        at(88, lambda: cap("M5_settle"))

    elif name == "SETTINGS":
        at(6, lambda: cap("S1_settings_early"))
        at(26, lambda: cap("S2_settings_mid"))
        at(30, lambda: learn("settings", "M5_settle", "S2_settings_mid"))
        at(46, lambda: q.click(*aim("settings", (W // 2, H // 2))))
        at(56, lambda: cap("S3_settings_click"))
        at(74, lambda: cap("S4_settings_late"))

    return P


def run_phase(ser, q, name, secs, W, H):
    print(f"\n===== PHASE {name} ({secs}s) =====", flush=True)
    snap(ser, f"{name}:start")
    t0 = time.time()
    for off, fn in phase_actions(name, q, W, H):
        wait = off - (time.time() - t0)
        if wait > 0:
            ser.pump(wait)
        elif wait < -3:
            print(f"  !! BEHIND SCHEDULE by {-wait:.1f}s at offset {off}",
                  flush=True)
        try:
            fn()
        except Exception as e:
            print(f"  !! action at +{off}s raised {e!r}", flush=True)
    left = secs - (time.time() - t0)
    if left > 0:
        ser.pump(left)
    snap(ser, f"{name}:end")
    print(f"  [drm] {drm_delta(f'{name}:start', f'{name}:end')}", flush=True)


# ------------------------------------------------------------------ main ----
MARK = re.compile(r"M12: MARK (\w+) (\d+)")


def main():
    global OUT
    OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m12"
    os.makedirs(OUT, exist_ok=True)
    ser = Serial(tee=os.path.join(OUT, "serial.log"))

    # Positive control. A harness that cannot see a command fail cannot tell
    # "absent" from "silent", and every finding below is a claim about one or
    # the other.
    print("=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    print(txt.strip()[-400:], flush=True)
    if not m:
        print(">>> CONTROL FAILED: the bogus command produced no failure text, "
              "so absence and failure are indistinguishable on this console. "
              "Aborting.", flush=True)
        sys.exit(4)
    print(f">>> CONTROL OK ({m.group(1)!r})\n", flush=True)

    ser.send("brush /bin/m12-caps")
    m, txt = ser.read_until(re.compile(r"M12: wayland-1 after (\d+)s"), 600)
    print(txt.strip()[-2500:], flush=True)
    if not m:
        print(">>> ABSENT: the compositor never bound wayland-1. Nothing to "
              "probe.", flush=True)
        sys.exit(2)
    print(f"\n>>> session up (wayland-1 after {m.group(1)}s)", flush=True)

    # Learn the real geometry from the first frame rather than assuming it: the
    # QMP absolute axes are scaled against it, and a wrong scale puts every
    # click somewhere other than where this script says it clicked.
    w, h, fb, missing, rects = vnc_capture()
    print(f">>> scanout is {w}x{h} (uncovered={missing})", flush=True)
    q = Qmp(w, h)
    if q.f is None:
        print(">>> NO QMP: input cannot be injected; the input, window-manage"
              "ment and keybinding phases will all report false negatives. "
              "Aborting rather than publishing them.", flush=True)
        sys.exit(3)

    while True:
        m, txt = ser.read_until(
            re.compile(r"M12: MARK (\w+) (\d+)|M12: CAPTURES DONE"), 400)
        if not m:
            print(">>> TIMEOUT waiting for the next MARK; the guest script "
                  "stopped early.", flush=True)
            break
        if m.group(0) == "M12: CAPTURES DONE":
            print("\n>>> guest reports CAPTURES DONE", flush=True)
            break
        run_phase(ser, q, m.group(1), int(m.group(2)), w, h)

    print(f"\n[qmp] {q.sent} commands sent, {q.rejected} rejected", flush=True)

    # Drain the guest's exfiltration dump into the tee. Everything after this
    # point is read off serial.log, not parsed here.
    print("\n>>> draining guest dump...", flush=True)
    ser.read_until(re.compile(r"M12: DONE"), 900)

    report(w, h)
    print("\n>>> serial log:", os.path.join(OUT, "serial.log"), flush=True)


def report(w, h):
    print("\n\n================ FRAME REPORT ================", flush=True)
    base = FRAMES.get("I1_idle_a")
    prev = None
    for name in ORDER:
        f = FRAMES[name]
        nd, top = f.census()
        dprev = diff(prev, f) if prev is not None else None
        dbase = diff(base, f) if base is not None and f is not base else None
        print(f"\n{name}")
        print(f"  uncovered={f.missing} colours={nd}")
        print("  top: " + "  ".join(f"0x{v:06x}:{c}" for v, c in top[:4]))
        if dprev is not None:
            print(f"  vs prev ({prev.name}): {boxstr(dprev)}")
        if dbase is not None:
            print(f"  vs base (I1_idle_a):  {boxstr(dbase)}")
        prev = f

    print("\n================ LIVENESS ================", flush=True)
    a, b, c = (FRAMES.get("I1_idle_a"), FRAMES.get("I2_idle_b"),
               FRAMES.get("I3_idle_c"))
    if a and b and c:
        d1, d2 = diff(a, b), diff(b, c)
        print(f"  I1->I2 {boxstr(d1)}")
        print(f"  I2->I3 {boxstr(d2)}")
        if d1["n"] == 0 and d2["n"] == 0:
            print("  VERDICT: the idle desktop is BYTE-FROZEN across ~34 s. "
                  "Every 'X changed nothing' below is therefore unreadable — "
                  "a frozen compositor and an ignored input look identical.")
        else:
            print("  VERDICT: the idle desktop advances on its own (the panel "
                  "clock). A later capture that is byte-identical to the one "
                  "before it is a stale capture; a later diff confined to this "
                  "region is a real 'nothing happened'.")


if __name__ == "__main__":
    main()
