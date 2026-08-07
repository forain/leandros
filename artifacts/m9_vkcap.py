#!/usr/bin/env python3
"""Photograph a Vulkan client (vkwl) presenting into cosmic-comp, through Venus.

This is venuscap.py's capture route pointed at a different subject. The route
itself is settled and not re-litigated here: `-display egl-headless` is a
GL->CPU *converter* whose egl_scanout_flush() blits into the 2D console
surface, a paired `-vnc ...,display=venusgpu` listener is the 2D consumer that
reads that surface, and `screendump` is structurally unable to (qemu_console_
surface() returns NULL for SCANOUT_TEXTURE, which is what virgl_cmd_set_scanout
sets). See ui/egl-headless.c, ui/console.c:1488, hw/display/virtio-gpu-virgl.c.

WHAT IS NEW HERE is the subject. drmsmoke reaches the scanout through the
dumb-BO/2D path; a Vulkan client reaches it through cosmic-comp, which
composites a wl_shm buffer Mesa filled by memcpy (Venus advertises no
VK_EXT_external_memory_host, so wsi_common_wayland picks WSI_WL_BUFFER_SHM_
MEMCPY for plain MESA_VK_WSI_DEBUG=sw). That those two ends meet at the same
virgl_cmd_set_scanout is an inference until this script measures it.

THREE CAPTURES, ONE BOOT:
  A  control  — desktop up, vkwl not yet started. Establishes what the scanout
                looks like WITHOUT the client, so any colour we later claim is
                vkwl's can be shown to be absent here.
  B  hold #1  — vkwl parked on its second-to-last frame.
  C  hold #2  — vkwl parked on its last frame, a DIFFERENT clear colour.
B-vs-C is the load-bearing comparison: one coloured rectangle appearing could
be a coincidence of the desktop; the same rectangle changing to a second
independently predicted colour cannot be.

The guest announces each phase on the serial console and prints its own
predicted 8-bit colour, so the harness never has to guess Mesa's or Vulkan's
UNORM rounding. Serial and VNC are separate channels; QMP/HMP is never opened.
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

# How close a captured pixel has to be to the client's own predicted 8-bit
# clear colour to count. Vulkan leaves UNORM rounding of the clear value up to
# 0.6 ULP to the implementation, and the client's prediction rounds half-up, so
# a channel can legitimately be one off; 2 leaves a little room for the
# compositor without letting a neighbouring desktop colour in.
TOL = 2


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
        # Drop whatever is already buffered. Boot and login chatter contains
        # phrases ("No such file") that a later sentinel regex would match, and
        # a control that can be satisfied by text predating the command is not
        # a control.
        self.buf = b""
        payload = (cmd + "\n").encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), 8):
            self.s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        self.s.setblocking(False)

    def read_until(self, pattern, timeout, label=""):
        """Read until `pattern` (compiled regex) matches somewhere in the
        not-yet-consumed buffer. Returns (match_or_None, consumed_text).

        The buffer persists across calls on purpose: a sentinel that lands
        while we are busy on the VNC socket must still be found afterwards,
        not lost.
        """
        end = time.time() + timeout
        while True:
            m = pattern.search(self.buf.decode("utf-8", "replace"))
            if m:
                txt = self.buf.decode("utf-8", "replace")
                cut = m.end()
                self.buf = txt[cut:].encode("utf-8", "replace")
                return m, txt[:cut]
            if time.time() >= end:
                txt = self.buf.decode("utf-8", "replace")
                return None, txt
            if select.select([self.s], [], [], 0.5)[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return None, self.buf.decode("utf-8", "replace")
                # brush's line editor asks for the cursor position; answer it
                # or the shell can sit waiting instead of running our command.
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

    # 32bpp true colour, little-endian, R=16 G=8 B=0, so '<I' reads 0x00RRGGBB
    # and compares directly against the RGB triple vkwl printed.
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
                        for col in range(rw):
                            seen[(ry + row) * w + rx + col] = 1
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


def write_ppm(path, w, h, fb):
    out = bytearray(w * h * 3)
    for i in range(w * h):
        out[i * 3 + 0] = fb[i * 4 + 2]
        out[i * 3 + 1] = fb[i * 4 + 1]
        out[i * 3 + 2] = fb[i * 4 + 0]
    with open(path, "wb") as f:
        f.write(f"P6\n{w} {h}\n255\n".encode())
        f.write(bytes(out))


# --------------------------------------------------------------- census ----
def pixels(w, h, fb):
    return [p & 0xFFFFFF for p in struct.unpack(f"<{w * h}I", fb)]


def census(label, w, h, px):
    hist = collections.Counter(px)
    print(f"--- census {label} ---")
    print(f"  resolution      : {w}x{h} ({w * h} px)")
    print(f"  distinct colours: {len(hist)}")
    print("  top 8:")
    for val, cnt in hist.most_common(8):
        print(f"      0x{val:06x}  {cnt:>9}  ({100.0 * cnt / (w * h):5.2f}%)")
    return hist


def find_colour(w, h, px, target, tol=TOL):
    """Count and locate pixels within `tol` per channel of `target`."""
    tr, tg, tb = (target >> 16) & 255, (target >> 8) & 255, target & 255
    lo_r, hi_r = tr - tol, tr + tol
    lo_g, hi_g = tg - tol, tg + tol
    lo_b, hi_b = tb - tol, tb + tol
    minx, miny, maxx, maxy = w, h, -1, -1
    n = 0
    exact = 0
    for i, p in enumerate(px):
        r = (p >> 16) & 255
        if r < lo_r or r > hi_r:
            continue
        g = (p >> 8) & 255
        if g < lo_g or g > hi_g:
            continue
        b = p & 255
        if b < lo_b or b > hi_b:
            continue
        n += 1
        if p == target:
            exact += 1
        y, x = divmod(i, w)
        if x < minx:
            minx = x
        if x > maxx:
            maxx = x
        if y < miny:
            miny = y
        if y > maxy:
            maxy = y
    box = None
    if n:
        box = (minx, miny, maxx, maxy, (maxx - minx + 1), (maxy - miny + 1))
    return n, exact, box


def report_colour(label, w, h, px, target, expect_area):
    n, exact, box = find_colour(w, h, px, target)
    print(f"  colour 0x{target:06x} +-{TOL}: {n} px (exact {exact})")
    fill = 0.0
    if box:
        minx, miny, maxx, maxy, bw, bh = box
        area = bw * bh
        fill = n / area
        print(f"      bbox        : x={minx}..{maxx} y={miny}..{maxy}  {bw}x{bh}")
        print(f"      bbox fill   : {fill:.4f}  (1.0 = a solid rectangle)")
    if expect_area:
        print(f"      vs swapchain: {n}/{expect_area} = "
              f"{100.0 * n / expect_area:.2f}% of the extent vkwl reported")
    return n, exact, box, fill


# ----------------------------------------------------------------- main ----
HOLD_RE = re.compile(
    r"VKWL: HOLD READY seq=(\d+) extent=(\d+)x(\d+) rgb=([0-9a-f]{6}) secs=(\d+)")


def main():
    outdir = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m9vk"
    frames = sys.argv[2] if len(sys.argv) > 2 else "304"
    hold = sys.argv[3] if len(sys.argv) > 3 else "150"
    presleep = sys.argv[4] if len(sys.argv) > 4 else "90"
    os.makedirs(outdir, exist_ok=True)

    ser = Serial(tee=os.path.join(outdir, "serial.log"))

    print("=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 20)
    print(txt.strip()[-500:], flush=True)
    if m:
        print(f">>> CONTROL OK: harness sees the bogus command fail "
              f"({m.group(1)!r})\n", flush=True)
    else:
        print(">>> CONTROL FAILED: bogus command produced no failure text. "
              "Absence and failure are not distinguishable on this console; "
              "aborting.\n", flush=True)
        sys.exit(4)

    cmd = f"brush /bin/m4-vkwl sw {frames} {hold} {presleep}"
    print(f"=== LAUNCH: {cmd} ===", flush=True)
    ser.send(cmd)

    # 1. Session up, vkwl not started -> control frame.
    m, txt = ser.read_until(re.compile(r"M4: control window opens now"), 900)
    print(txt.strip()[-3000:], flush=True)
    if not m:
        print("\n>>> ABSENT: 'M4: control window opens now' never arrived. "
              "The COSMIC session did not come up; nothing to photograph.",
              flush=True)
        sys.exit(2)
    print("\n>>> session ready; capturing CONTROL\n", flush=True)
    time.sleep(3)
    capA = vnc_capture()
    print(f"    capture A: {capA[0]}x{capA[1]} name={capA[2]!r} "
          f"rects={capA[5]} uncovered={capA[4]}", flush=True)

    # 2. vkwl parked on each of its last two frames.
    holds = []
    for phase in ("B", "C"):
        m, txt = ser.read_until(HOLD_RE, 1200)
        print(txt.strip()[-4000:], flush=True)
        if not m:
            print(f"\n>>> ABSENT: no 'VKWL: HOLD READY' for phase {phase}.",
                  flush=True)
            break
        seq, ew, eh, rgb, secs = (int(m.group(1)), int(m.group(2)),
                                  int(m.group(3)), int(m.group(4), 16),
                                  int(m.group(5)))
        print(f"\n>>> HOLD {phase}: seq={seq} extent={ew}x{eh} "
              f"predicted rgb=0x{rgb:06x} for {secs}s; capturing\n", flush=True)
        time.sleep(4)
        cap = vnc_capture()
        print(f"    capture {phase}: {cap[0]}x{cap[1]} rects={cap[5]} "
              f"uncovered={cap[4]}", flush=True)
        holds.append((phase, seq, ew, eh, rgb, cap))

    # 3. Analysis.
    print("\n\n================ ANALYSIS ================", flush=True)
    wA, hA, _, fbA, missA, _ = capA
    write_ppm(os.path.join(outdir, "capA-control.ppm"), wA, hA, fbA)
    pxA = pixels(wA, hA, fbA)
    print(f"capture A uncovered pixels: {missA}")
    census("A / control (no vkwl)", wA, hA, pxA)

    verdicts = []
    for phase, seq, ew, eh, rgb, cap in holds:
        w, h, _, fb, miss, _ = cap
        write_ppm(os.path.join(outdir, f"cap{phase}-seq{seq}.ppm"), w, h, fb)
        px = pixels(w, h, fb)
        print(f"\ncapture {phase} uncovered pixels: {miss}")
        census(f"{phase} / vkwl seq={seq}", w, h, px)
        print(f"  vkwl swapchain extent {ew}x{eh} = {ew * eh} px")
        n, exact, box, fill = report_colour(phase, w, h, px, rgb, ew * eh)
        nA, _, _ = find_colour(wA, hA, pxA, rgb)
        print(f"  same colour in CONTROL A: {nA} px")
        swapped = ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)
        nsw, _, _ = find_colour(w, h, px, swapped)
        print(f"  byte-swapped 0x{swapped:06x} (channel-order check): {nsw} px")
        same_as_A = (fb == fbA)
        print(f"  byte-identical to control A: {same_as_A}")
        verdicts.append(dict(phase=phase, seq=seq, rgb=rgb, area=ew * eh,
                             n=n, exact=exact, fill=fill, nA=nA,
                             same_as_A=same_as_A, miss=miss))

    if len(holds) == 2:
        fbB = holds[0][5][3]
        fbC = holds[1][5][3]
        print(f"\nB and C byte-identical to each other: {fbB == fbC}")

    print("\n---------------- VERDICT ----------------")
    if missA:
        print("capture A incomplete; treat everything below as provisional.")
    ok = bool(verdicts)
    for v in verdicts:
        crit = {
            "coverage >= 90% of swapchain extent":
                v["area"] and v["n"] >= 0.90 * v["area"],
            "region is a solid rectangle (bbox fill >= 0.95)": v["fill"] >= 0.95,
            "colour absent from the no-vkwl control": v["nA"] == 0,
            "frame differs from the no-vkwl control": not v["same_as_A"],
            "capture complete": v["miss"] == 0,
        }
        print(f"\nphase {v['phase']} (seq={v['seq']}, "
              f"predicted 0x{v['rgb']:06x}):")
        for k, val in crit.items():
            print(f"    [{'PASS' if val else 'FAIL'}] {k}")
        ok = ok and all(crit.values())
    if len(verdicts) != 2:
        print("\n    [FAIL] both hold phases captured")
        ok = False
    print(f"\nOVERALL: {'PASS' if ok else 'FAIL'}")

    # Drain the rest of the run (cosmic.log tail, M4: DONE) into the tee.
    ser.read_until(re.compile(r"M4: DONE"), 400)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
