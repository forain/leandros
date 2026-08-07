#!/usr/bin/env python3
"""Photograph a Venus/virgl GL scanout by pairing egl-headless with VNC.

Why this exists (established from QEMU 11.0.1 source, not inferred):

  * ui/egl-headless.c's dpy_gl_update handler, egl_scanout_flush(), ends in
    `egl_fb_read(edpy->ds, &edpy->blit_fb)` + `dpy_gfx_update(...)`. That is a
    real GL->CPU readback into the 2D console surface. egl-headless is a
    *converter*, not a sink; egl_is_compatible_dcl() states the design outright.
  * hw/display/virtio-gpu-virgl.c's virgl_cmd_set_scanout() calls
    qemu_console_resize() (which allocates a correctly sized x8r8g8b8
    DisplaySurface) and then dpy_gl_scanout_texture(), which sets
    console->scanout.kind = SCANOUT_TEXTURE.
  * ui/console.c's qemu_console_surface() returns NULL for every scanout.kind
    except SCANOUT_SURFACE. ui/ui-qmp-cmds.c's qmp_screendump() calls it and
    bails with "no surface".

So the pixels ARE in con->surface; screendump's accessor just refuses them.
A VNC listener bound to the same console reads that surface directly, so this
script fetches the frame over RFB instead.

Serial and VNC are separate channels; QMP/HMP is never opened here, so the
one-client rule is not violated.
"""

import socket
import struct
import sys
import time
import select
import collections

SERIAL_SOCK = "/tmp/leandros-serial.sock"
VNC_HOST, VNC_PORT = "127.0.0.1", 5909

# Pre-committed expected frame from drmsmoke --hold (commit 9d73b43):
# full-screen 0x181818 with a 256x256 0xFF0000 block at (64,64).
FIELD = 0x181818
BLOCK = 0xFF0000
BLOCK_X, BLOCK_Y, BLOCK_W, BLOCK_H = 64, 64, 256, 256


# ---------------------------------------------------------------- serial ----
class Serial:
    def __init__(self):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SERIAL_SOCK)
        self.s.setblocking(False)
        self.drain(0.5)

    def drain(self, secs):
        end = time.time() + secs
        while time.time() < end:
            if select.select([self.s], [], [], 0.1)[0]:
                try:
                    if not self.s.recv(65536):
                        return
                except BlockingIOError:
                    pass

    def send(self, cmd):
        payload = (cmd + "\n").encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), 8):
            self.s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        self.s.setblocking(False)

    def read_until(self, sentinel, timeout):
        """Read until sentinel appears. Returns (found, text)."""
        buf = b""
        end = time.time() + timeout
        while time.time() < end:
            if select.select([self.s], [], [], 0.2)[0]:
                try:
                    chunk = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not chunk:
                    break
                buf += chunk
                if b"\x1b[6n" in chunk:
                    self.s.setblocking(True)
                    self.s.sendall(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
                    self.s.setblocking(False)
                if sentinel.encode() in buf:
                    return True, buf.decode("utf-8", "replace")
        return False, buf.decode("utf-8", "replace")


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
    s.settimeout(10)

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
    recvall(s, 16)  # server pixel format (we override it below)
    nlen = struct.unpack(">I", recvall(s, 4))[0]
    name = recvall(s, nlen).decode("utf-8", "replace")

    # Force 32bpp true-colour, little-endian, shifts R=16 G=8 B=0.  Each pixel
    # then reads back with '<I' as 0x00RRGGBB, directly comparable to the
    # constants drmsmoke paints.
    pixfmt = struct.pack(">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
    s.sendall(b"\x00\x00\x00\x00" + pixfmt)
    s.sendall(b"\x02\x00" + struct.pack(">H", 1) + struct.pack(">i", 0))  # Raw only

    fb = bytearray(w * h * 4)
    seen = bytearray(w * h)
    end = time.time() + timeout
    rects_total = 0

    while time.time() < end:
        s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))  # non-incremental
        got_update = False
        inner = time.time() + 12
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
    missing = len(seen) - sum(seen)
    return w, h, name, bytes(fb), missing, rects_total


# ---------------------------------------------------------------- verify ----
def analyse(w, h, fb):
    px = struct.unpack(f"<{w * h}I", fb)
    px = [p & 0xFFFFFF for p in px]
    hist = collections.Counter(px)

    print(f"resolution        : {w}x{h}  ({w * h} px)")
    print("colour histogram  :")
    for val, cnt in hist.most_common(8):
        print(f"    0x{val:06x}  {cnt}")
    if len(hist) > 8:
        print(f"    ... {len(hist) - 8} more distinct colours")
    print(f"distinct colours  : {len(hist)}")

    n_block = hist.get(BLOCK, 0)
    n_field = hist.get(FIELD, 0)
    print(f"0x{BLOCK:06x} count    : {n_block}   (expected {BLOCK_W * BLOCK_H})")
    print(f"0x{FIELD:06x} count    : {n_field}   "
          f"(expected {w * h - BLOCK_W * BLOCK_H})")

    def at(x, y):
        return px[y * w + x]

    corners = {
        "in  (64,64)": at(BLOCK_X, BLOCK_Y),
        "in  (319,319)": at(BLOCK_X + BLOCK_W - 1, BLOCK_Y + BLOCK_H - 1),
        "out (63,64)": at(BLOCK_X - 1, BLOCK_Y),
        "out (64,63)": at(BLOCK_X, BLOCK_Y - 1),
        "out (320,319)": at(BLOCK_X + BLOCK_W, BLOCK_Y + BLOCK_H - 1),
        "out (319,320)": at(BLOCK_X + BLOCK_W - 1, BLOCK_Y + BLOCK_H),
    }
    print("block corners     :")
    for k, v in corners.items():
        print(f"    {k:>14} = 0x{v:06x}")

    ok = (
        n_block == BLOCK_W * BLOCK_H
        and n_field == w * h - BLOCK_W * BLOCK_H
        and len(hist) == 2
        and corners["in  (64,64)"] == BLOCK
        and corners["in  (319,319)"] == BLOCK
        and all(corners[k] == FIELD for k in corners if k.startswith("out"))
    )
    print(f"\nVERDICT: {'EXACT MATCH' if ok else 'MISMATCH'}")
    return ok


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else "?"
    ppm_out = sys.argv[2] if len(sys.argv) > 2 else "/tmp/venuscap.ppm"

    ser = Serial()

    print("=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    time.sleep(3)
    _, ctrl = ser.read_until("\x00NEVER\x00", 5)
    print(ctrl.strip()[-600:], flush=True)
    lowered = ctrl.lower()
    if "not found" in lowered or "no such" in lowered or "cannot" in lowered:
        print(">>> CONTROL OK: harness reports the bogus command failing\n", flush=True)
    else:
        print(">>> CONTROL INCONCLUSIVE — inspect the text above\n", flush=True)

    print("=== drmsmoke --hold ===", flush=True)
    ser.send("drmsmoke --hold")
    found, text = ser.read_until("DRMSMOKE: HOLD READY", 180)
    print(text.strip()[-2500:], flush=True)
    if not found:
        print("\n>>> ABSENT: sentinel 'DRMSMOKE: HOLD READY' never arrived", flush=True)
        sys.exit(2)
    print("\n>>> sentinel seen; scanout is held\n", flush=True)

    time.sleep(3)  # let a DIRTYFB round trip through egl_scanout_flush

    print("=== VNC CAPTURE (127.0.0.1:5909, console=venusgpu) ===", flush=True)
    w, h, name, fb, missing, rects = vnc_capture()
    print(f"server name       : {name}")
    print(f"rects received    : {rects}")
    print(f"uncovered pixels  : {missing}")
    if missing:
        print(">>> INCOMPLETE CAPTURE", flush=True)
        sys.exit(3)

    with open(ppm_out, "wb") as f:
        f.write(f"P6\n{w} {h}\n255\n".encode())
        out = bytearray(w * h * 3)
        for i in range(w * h):
            out[i * 3 + 0] = fb[i * 4 + 2]
            out[i * 3 + 1] = fb[i * 4 + 1]
            out[i * 3 + 2] = fb[i * 4 + 0]
        f.write(bytes(out))
    print(f"ppm written       : {ppm_out}\n")

    ok = analyse(w, h, fb)
    print(f"arch              : {arch}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
