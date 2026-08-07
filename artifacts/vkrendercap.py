#!/usr/bin/env python3
"""Photograph `vkrender --present` reaching the scanout, with no compositor.

This is venuscap.py's capture route (settled, not re-litigated here: egl-headless
is a GL->CPU converter whose egl_scanout_flush() blits into the 2D console
surface; a paired `-vnc ...,display=venusgpu` listener is the 2D consumer;
`screendump` is structurally unable to, because qemu_console_surface() returns
NULL for the SCANOUT_TEXTURE that virgl_cmd_set_scanout sets) pointed at a
different subject: a Vulkan-rendered image blitted into a DRM dumb BO and
SETCRTC'd directly, no Wayland and no compositor anywhere in the chain.

THREE CAPTURES, ONE BOOT, in this order on purpose:
  A  control   — no DRM client has run yet. Whatever vkrender's colours are,
                 they have to be absent here.
  B  subject   — vkrender parked in its --present-hold-ms nanosleep.
  C  reference — drmsmoke --hold, whose aarch64 frame is already pinned on this
                 box (1280x800, exactly 2 colours, 65536 x FF0000, 958464 x
                 181818). C runs LAST so it cannot contaminate B, and it is what
                 separates "B is wrong" from "the camera was broken this boot".

Pass criteria were committed to
artifacts/notes/m9-vkrender-aarch64/precommit-pass-criteria.txt before QEMU was
started (git 8634425). The load-bearing one is P7: the on-screen census must
equal the s2_coverage triple the same run printed from its own CPU-side
readback of the rendered image, exactly.

Serial and VNC are separate channels; QMP/HMP is never opened here, so the
one-client rule is not violated.
"""

import collections
import os
import re
import select
import socket
import struct
import sys
import time

SERIAL_SOCK = "/tmp/leandros-serial.sock"
VNC_HOST, VNC_PORT = "127.0.0.1", 5909

FIELD = 0x181818          # do_present's background, and drmsmoke's
TRI = 0xFF0000            # TRI_RGBA  {FF,00,00,FF} after the XRGB swap
CLEAR = 0x0000FF          # CLEAR_RGBA {00,00,FF,FF} after the XRGB swap
IMG_W = IMG_H = 256
TRI_AREA_PX, TRI_AREA_TOL = 18432, 600
PINNED_CHECKSUM = 0x02C0FDC5


# ---------------------------------------------------------------- serial ----
class Serial:
    def __init__(self, tee=None):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SERIAL_SOCK)
        self.s.setblocking(False)
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

    def read_until(self, pattern, timeout):
        """Read until `pattern` (compiled regex) matches the unconsumed buffer.
        The buffer persists across calls: a sentinel that lands while we are
        busy on the VNC socket must still be findable afterwards."""
        end = time.time() + timeout
        while True:
            txt = self.buf.decode("utf-8", "replace")
            m = pattern.search(txt)
            if m:
                self.buf = txt[m.end():].encode("utf-8", "replace")
                return m, txt[:m.end()]
            if time.time() >= end:
                return None, txt
            if select.select([self.s], [], [], 0.5)[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return None, self.buf.decode("utf-8", "replace")
                if b"\x1b[6n" in c:
                    self.s.setblocking(True)
                    self.s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                    self.s.setblocking(False)
                self._stash(c)


# ------------------------------------------------------------------- rfb ----
def recvall(s, n):
    out = b""
    while len(out) < n:
        chunk = s.recv(n - len(out))
        if not chunk:
            raise EOFError(f"VNC closed with {len(out)}/{n} bytes")
        out += chunk
    return out


def vnc_capture(timeout=90.0):
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
        raise RuntimeError(f"VNC needs auth; offered types {list(types)}")
    s.sendall(bytes([1]))
    result = struct.unpack(">I", recvall(s, 4))[0]
    if result != 0:
        rlen = struct.unpack(">I", recvall(s, 4))[0]
        raise RuntimeError(f"VNC auth failed: {recvall(s, rlen)!r}")

    s.sendall(bytes([1]))  # ClientInit, shared
    w, h = struct.unpack(">HH", recvall(s, 4))
    recvall(s, 16)
    nlen = struct.unpack(">I", recvall(s, 4))[0]
    name = recvall(s, nlen).decode("utf-8", "replace")

    # 32bpp true colour, little-endian, R=16 G=8 B=0, so '<I' reads 0x00RRGGBB.
    pixfmt = struct.pack(">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
    s.sendall(b"\x00\x00\x00\x00" + pixfmt)
    s.sendall(b"\x02\x00" + struct.pack(">H", 1) + struct.pack(">i", 0))  # Raw

    fb = bytearray(w * h * 4)
    seen = bytearray(w * h)
    end = time.time() + timeout
    rects_total = 0

    while time.time() < end:
        s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))  # non-incremental
        got_update = False
        inner = time.time() + 15
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
                        base = (ry + row) * w + rx
                        seen[base:base + rw] = b"\x01" * rw
                    rects_total += 1
                got_update = True
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
        if all(seen):
            break
        if not got_update:
            time.sleep(0.3)

    s.close()
    return w, h, name, bytes(fb), len(seen) - sum(seen), rects_total


# --------------------------------------------------------------- analysis ---
def pixels(w, h, fb):
    return [p & 0xFFFFFF for p in struct.unpack(f"<{w * h}I", fb)]


def write_ppm(path, w, h, fb):
    out = bytearray(w * h * 3)
    for i in range(w * h):
        out[i * 3 + 0] = fb[i * 4 + 2]
        out[i * 3 + 1] = fb[i * 4 + 1]
        out[i * 3 + 2] = fb[i * 4 + 0]
    with open(path, "wb") as f:
        f.write(f"P6\n{w} {h}\n255\n".encode())
        f.write(bytes(out))


def census(label, w, h, px, top=10):
    hist = collections.Counter(px)
    print(f"\n  --- census: {label} ---")
    print(f"  resolution      : {w}x{h}  ({w * h} px)")
    print(f"  distinct colours: {len(hist)}")
    for val, cnt in hist.most_common(top):
        print(f"      0x{val:06x}  {cnt:>9}  ({100.0 * cnt / (w * h):6.3f}%)")
    if len(hist) > top:
        print(f"      ... {len(hist) - top} more distinct colours")
    return hist


def bbox(w, h, px, pred):
    minx, miny, maxx, maxy, n = w, h, -1, -1, 0
    for i, p in enumerate(px):
        if not pred(p):
            continue
        n += 1
        y, x = divmod(i, w)
        if x < minx: minx = x
        if x > maxx: maxx = x
        if y < miny: miny = y
        if y > maxy: maxy = y
    if n == 0:
        return None
    bw, bh = maxx - minx + 1, maxy - miny + 1
    return dict(n=n, minx=minx, miny=miny, maxx=maxx, maxy=maxy,
                bw=bw, bh=bh, fill=n / (bw * bh))


def show_box(label, b):
    if b is None:
        print(f"  {label}: ABSENT (0 px)")
        return
    print(f"  {label}: {b['n']} px  bbox x={b['minx']}..{b['maxx']} "
          f"y={b['miny']}..{b['maxy']}  {b['bw']}x{b['bh']}  "
          f"fill={b['fill']:.4f}")


# ------------------------------------------------------------------- main ---
RE_FAIL = re.compile(r"(not found|No such file|command not found|cannot)", re.I)
RE_MODE = re.compile(r"present: mode (\d+)x(\d+)")
RE_HOLD = re.compile(r"present: holding the image for (\d+) ms")
RE_COV = re.compile(
    r"s2_coverage: triangle=(\d+) clear=(\d+) other=(\d+) \(total (\d+)\)")
RE_SUM = re.compile(r"s2_checksum: FNV-1a over \d+ bytes = (0x[0-9A-Fa-f]+)")
RE_DONE = re.compile(r"--- vkrender done, failures = (\d+), skipped = (\d+) ---")
RE_DRMHOLD = re.compile(r"DRMSMOKE: HOLD READY")


def main():
    outdir = sys.argv[1] if len(sys.argv) > 1 else "/tmp/vkrcap"
    hold_ms = sys.argv[2] if len(sys.argv) > 2 else "240000"
    os.makedirs(outdir, exist_ok=True)
    ser = Serial(tee=os.path.join(outdir, "serial.log"))
    t0 = time.time()

    def el():
        return f"[t+{time.time() - t0:7.1f}s]"

    # ---- harness control -------------------------------------------------
    print("=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(RE_FAIL, 30)
    print(txt.strip()[-400:], flush=True)
    if not m:
        print(">>> CONTROL FAILED: bogus command produced no failure text. "
              "Absence and failure are not distinguishable on this console; "
              "aborting.", flush=True)
        sys.exit(4)
    print(f">>> CONTROL OK ({m.group(1)!r})\n", flush=True)

    # ---- A: control, no DRM client has run -------------------------------
    print(f"{el()} === CAPTURE A (control, no DRM client) ===", flush=True)
    capA = vnc_capture()
    print(f"    A: {capA[0]}x{capA[1]} name={capA[2]!r} rects={capA[5]} "
          f"uncovered={capA[4]}", flush=True)

    # ---- B: vkrender --present -------------------------------------------
    cmd = f"vkrender --present --present-hold-ms={hold_ms}"
    print(f"\n{el()} === LAUNCH: {cmd} ===", flush=True)
    ser.send(cmd)
    m, txt = ser.read_until(RE_HOLD, 3600)
    print(txt.strip()[-6000:], flush=True)
    if not m:
        print(f"\n{el()} >>> ABSENT: vkrender never reached its --present hold. "
              "See the serial text above for how far it got.", flush=True)
        sys.exit(2)
    print(f"\n{el()} >>> holding for {m.group(1)} ms; capturing B\n", flush=True)

    consumed = txt
    mm = RE_MODE.search(consumed)
    mode_w, mode_h = (int(mm.group(1)), int(mm.group(2))) if mm else (None, None)
    mc = RE_COV.search(consumed)
    cov = tuple(int(g) for g in mc.groups()) if mc else None
    ms = RE_SUM.search(consumed)
    checksum = int(ms.group(1), 16) if ms else None
    present_lines = [l.strip() for l in consumed.splitlines()
                     if re.match(r"\s*(present|s2)_\w+:\s+(PASS|FAIL|SKIP)", l)]

    time.sleep(5)
    capB = vnc_capture()
    print(f"    B: {capB[0]}x{capB[1]} rects={capB[5]} uncovered={capB[4]}",
          flush=True)

    # let vkrender finish its hold and exit
    m, txt2 = ser.read_until(RE_DONE, int(hold_ms) / 1000 + 600)
    print(txt2.strip()[-2500:], flush=True)
    done = (int(m.group(1)), int(m.group(2))) if m else None
    if checksum is None:
        ms = RE_SUM.search(consumed + txt2)
        checksum = int(ms.group(1), 16) if ms else None

    # ---- C: drmsmoke --hold, the pinned reference ------------------------
    print(f"\n{el()} === REFERENCE: drmsmoke --hold ===", flush=True)
    ser.send("drmsmoke --hold")
    m, txt3 = ser.read_until(RE_DRMHOLD, 900)
    print(txt3.strip()[-2000:], flush=True)
    capC = None
    if not m:
        print(f"\n{el()} >>> drmsmoke never held; reference C unavailable.",
              flush=True)
    else:
        time.sleep(5)
        capC = vnc_capture()
        print(f"    C: {capC[0]}x{capC[1]} rects={capC[5]} "
              f"uncovered={capC[4]}", flush=True)

    # ---- analysis --------------------------------------------------------
    print("\n\n================ ANALYSIS ================", flush=True)
    print(f"serial-reported mode      : {mode_w}x{mode_h}")
    print(f"serial s2_coverage        : {cov}")
    print(f"serial s2_checksum        : "
          f"{'0x%08X' % checksum if checksum is not None else None}")
    print(f"serial vkrender done      : failures={done[0] if done else '?'} "
          f"skipped={done[1] if done else '?'}")
    print("serial present_*/s2_* lines:")
    for l in present_lines:
        print(f"    {l}")

    wA, hA, _, fbA, missA, _ = capA
    write_ppm(os.path.join(outdir, "capA-control.ppm"), wA, hA, fbA)
    pxA = pixels(wA, hA, fbA)
    census("A / control (no DRM client)", wA, hA, pxA)
    histA = collections.Counter(pxA)

    wB, hB, _, fbB, missB, _ = capB
    write_ppm(os.path.join(outdir, "capB-vkrender.ppm"), wB, hB, fbB)
    pxB = pixels(wB, hB, fbB)
    histB = census("B / vkrender --present", wB, hB, pxB)

    histC = None
    if capC:
        wC, hC, _, fbC, missC, _ = capC
        write_ppm(os.path.join(outdir, "capC-drmsmoke.ppm"), wC, hC, fbC)
        pxC = pixels(wC, hC, fbC)
        histC = census("C / drmsmoke --hold (pinned reference)", wC, hC, pxC)

    print("\n  --- geometry of B ---")
    b_img = bbox(wB, hB, pxB, lambda p: p != FIELD)
    show_box("non-background (expect 256x256 at ox,oy)", b_img)
    b_tri = bbox(wB, hB, pxB, lambda p: p == TRI)
    show_box("0xff0000 triangle", b_tri)
    b_clr = bbox(wB, hB, pxB, lambda p: p == CLEAR)
    show_box("0x0000ff clear", b_clr)

    ox = (wB - IMG_W) // 2 if wB > IMG_W else 0
    oy = (hB - IMG_H) // 2 if hB > IMG_H else 0
    print(f"  predicted centring: ox={ox} oy={oy} -> "
          f"x={ox}..{ox + IMG_W - 1} y={oy}..{oy + IMG_H - 1}")

    n_tri = histB.get(TRI, 0)
    n_clr = histB.get(CLEAR, 0)
    n_fld = histB.get(FIELD, 0)

    print("\n---------------- VERDICT ----------------")
    crit = {}
    crit["P1 resolution == the mode vkrender printed"] = (
        mode_w is not None and (wB, hB) == (mode_w, mode_h))
    crit["P2 exactly 3 distinct colours {181818, 0000ff, ff0000}"] = (
        set(histB) == {FIELD, CLEAR, TRI})
    crit["P3 count(0x181818) == W*H - 65536"] = (n_fld == wB * hB - 65536)
    crit["P4 count(ff0000)+count(0000ff) == 65536"] = (n_tri + n_clr == 65536)
    crit["P5 non-background bbox is exactly 256x256 at (ox,oy), fill 1.0"] = (
        b_img is not None and (b_img["minx"], b_img["miny"]) == (ox, oy)
        and (b_img["bw"], b_img["bh"]) == (IMG_W, IMG_H)
        and b_img["fill"] == 1.0)
    crit[f"P6 count(ff0000) within {TRI_AREA_PX} +/- {TRI_AREA_TOL}"] = (
        abs(n_tri - TRI_AREA_PX) <= TRI_AREA_TOL)
    crit["P7 census == the guest's own s2_coverage triple, exactly"] = (
        cov is not None and n_tri == cov[0] and n_clr == cov[1] and cov[2] == 0)
    crit["P8 s2_checksum == pinned 0x02C0FDC5"] = (checksum == PINNED_CHECKSUM)
    crit["P9 present_setcrtc and present_addfb2 both PASS"] = (
        any("present_setcrtc: PASS" in l for l in present_lines)
        and any("present_addfb2: PASS" in l for l in present_lines))
    crit["P10 capture B complete (0 uncovered)"] = (missB == 0)
    crit["A control: 0x0000ff absent before any DRM client"] = (
        histA.get(CLEAR, 0) == 0)
    crit["A control: B is not byte-identical to A"] = (fbB != fbA)
    if histC is not None:
        crit["C reference: drmsmoke census matches its pinned aarch64 frame"] = (
            len(histC) == 2 and histC.get(TRI, 0) == 65536
            and histC.get(FIELD, 0) == wC * hC - 65536)
        crit["C reference: 0x0000ff absent from drmsmoke's frame"] = (
            histC.get(CLEAR, 0) == 0)
        crit["C reference: B differs from C"] = (fbB != fbC)

    for k, v in crit.items():
        print(f"  [{'PASS' if v else 'FAIL'}] {k}")
    ok = all(crit.values())
    print(f"\nVERDICT: {'aarch64 VULKAN-TO-SCANOUT CONFIRMED' if ok else 'NOT PROVEN'}")
    print(f"ppm files in {outdir}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
