#!/usr/bin/env python3
"""Photograph a Vulkan client presenting into cosmic-comp on aarch64, and time
every step of getting there.

This is m9_vkcap.py (the x86_64 M4 capture) with the same capture route, the
same census and the same verdict, plus two things aarch64/TCG needs:

  * EVERY sentinel is stamped with the host's own wall clock, relative to the
    launch. The open question on aarch64 was never whether Venus works — a
    51-subtest vkrender run reached its present hold 7.1 s after the command
    was sent — it was whether a COSMIC compositor comes up in a usable time
    under TCG. A run that does not finish is only worth something if it says
    WHERE it stopped and after how many seconds, so the timings are the
    primary product here and the pixels are the second one.

  * The guest-side driver is m4-vkwl-a64, whose first argument chooses
    `comp` (cosmic-comp alone) or `session` (the full COSMIC session, i.e.
    exactly what x86_64 ran). Those are different experiments and the harness
    records which one it drove.

The capture route itself is settled and not re-litigated: `-display
egl-headless` blits the guest's GL scanout into the 2D console surface and a
paired `-vnc ...,display=venusgpu` listener is the 2D consumer that reads it.
`screendump` is structurally unable to do this at any device= (
qemu_console_surface() returns NULL for SCANOUT_TEXTURE, which is what
virgl_cmd_set_scanout sets).

Pass criteria were written and committed BEFORE the first capture:
artifacts/notes/m9-vkwl-aarch64/precommit-pass-criteria.txt.
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

# Per-channel slack against the client's own predicted 8-bit clear colour.
# Vulkan leaves UNORM rounding of a clear value up to 0.6 ULP to the
# implementation and the client's prediction rounds half-up, so one channel can
# legitimately be one off. Same value the x86_64 run used.
TOL = 2

T0 = time.time()


def stamp():
    return f"[t+{time.time() - T0:7.1f}s]"


def say(msg):
    print(f"{stamp()} {msg}", flush=True)


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
        # Drop whatever is already buffered: boot and login chatter contains
        # phrases a later sentinel regex would match, and a control that can be
        # satisfied by text predating the command is not a control.
        self.buf = b""
        payload = (cmd + "\n").encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), 8):
            self.s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        self.s.setblocking(False)

    def read_until(self, pattern, timeout, echo=None):
        """Read until `pattern` matches in the not-yet-consumed buffer.

        `echo` is a second pattern whose every match is printed with a host
        timestamp as it arrives — that is how the compositor's per-minute
        heartbeat becomes a measurement instead of log noise. The buffer
        persists across calls on purpose: a sentinel that lands while we are
        busy on the VNC socket must still be findable afterwards.
        """
        end = time.time() + timeout
        echoed = 0
        while True:
            txt = self.buf.decode("utf-8", "replace")
            if echo:
                lines = txt.splitlines()
                for ln in lines[echoed:]:
                    if echo.search(ln):
                        say(f"    | {ln.strip()}")
                echoed = max(0, len(lines) - 1)
            m = pattern.search(txt)
            if m:
                cut = m.end()
                self.buf = txt[cut:].encode("utf-8", "replace")
                return m, txt[:cut]
            if time.time() >= end:
                return None, txt
            if select.select([self.s], [], [], 0.5)[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return None, self.buf.decode("utf-8", "replace")
                # brush's line editor asks for the cursor position; answer it or
                # the shell can sit waiting instead of running our command.
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


def vnc_capture(timeout=120.0):
    s = socket.create_connection((VNC_HOST, VNC_PORT), timeout=15)
    s.settimeout(30)

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
        inner = time.time() + 30
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
        minx = min(minx, x)
        maxx = max(maxx, x)
        miny = min(miny, y)
        maxy = max(maxy, y)
    box = None
    if n:
        box = (minx, miny, maxx, maxy, maxx - minx + 1, maxy - miny + 1)
    return n, exact, box


def report_colour(w, h, px, target, expect_area):
    n, exact, box = find_colour(w, h, px, target)
    print(f"  colour 0x{target:06x} +-{TOL}: {n} px (exact {exact})")
    fill = 0.0
    bbox = None
    if box:
        minx, miny, maxx, maxy, bw, bh = box
        fill = n / (bw * bh)
        bbox = (minx, miny, maxx, maxy)
        print(f"      bbox        : x={minx}..{maxx} y={miny}..{maxy}  {bw}x{bh}")
        print(f"      bbox fill   : {fill:.4f}  (1.0 = a solid rectangle)")
    if expect_area:
        print(f"      vs swapchain: {n}/{expect_area} = "
              f"{100.0 * n / expect_area:.2f}% of the extent vkwl reported")
    return n, exact, bbox, fill


# ------------------------------------------------------------ hold sample ----
def sample_hold(phase, rgb, first, every, budget):
    """Capture repeatedly during one hold until the client's own predicted
    colour is on the scanout, and time how long that took.

    Run 1 sampled each hold exactly once, six seconds after the sentinel, and
    that is not enough. `VKWL: HOLD READY seq=N` means the CLIENT has returned
    from vkQueuePresentKHR for frame N; it does not mean the compositor has
    composited that buffer and the scanout has been flipped to it. In run 1 the
    seq=26 sample caught vkwl's OWN PREVIOUS frame — cols[1], 151868 px, in the
    identical 478x318 bounding box at identical 0.9991 fill — while the seq=27
    sample, taken after 180 s of idle, was current. One sample cannot tell
    "the pixels never arrive" from "the pixels had not arrived YET", and those
    are opposite conclusions.

    So the sample becomes a series and the answer becomes a number: the delay
    from the sentinel to the first frame carrying the predicted colour. The
    first sample is kept and reported whatever it shows — this is a wider
    aperture, not a retry until the answer is nice.
    """
    t_ready = time.time()
    time.sleep(first)
    samples = []
    while True:
        cap = vnc_capture()
        dt = time.time() - t_ready
        w, h, _, fb, miss, rects = cap
        n, _, _ = find_colour(w, h, pixels(w, h, fb), rgb)
        samples.append((dt, cap, n))
        say(f"    sample {phase}#{len(samples)} at +{dt:5.1f}s: {w}x{h} "
            f"rects={rects} uncovered={miss} predicted 0x{rgb:06x} -> {n} px")
        if n:
            return samples, len(samples) - 1
        if time.time() - t_ready > budget:
            say(f"    >>> {phase}: predicted colour never appeared within "
                f"{budget}s of the sentinel; keeping the first sample")
            return samples, 0
        time.sleep(every)


# ----------------------------------------------------------------- main ----
HOLD_RE = re.compile(
    r"VKWL: HOLD READY seq=(\d+) extent=(\d+)x(\d+) rgb=([0-9a-f]{6}) secs=(\d+)")
HEARTBEAT_RE = re.compile(r"M4A: still waiting|M4A: WAYLAND-1|M4A: SETTLE|"
                          r"M4A: COMP LAUNCH|VKWL: HOLD END|panic|VKWL: ERROR")


def main():
    outdir = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m9vk-a64"
    mode = sys.argv[2] if len(sys.argv) > 2 else "comp"
    frames = sys.argv[3] if len(sys.argv) > 3 else "28"
    hold = sys.argv[4] if len(sys.argv) > 4 else "180"
    presleep = sys.argv[5] if len(sys.argv) > 5 else "60"
    settle = sys.argv[6] if len(sys.argv) > 6 else "30"
    maxwait = sys.argv[7] if len(sys.argv) > 7 else "1800"
    os.makedirs(outdir, exist_ok=True)

    ser = Serial(tee=os.path.join(outdir, "serial.log"))

    say("=== POSITIVE CONTROL (must FAIL) ===")
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 60)
    print(txt.strip()[-400:], flush=True)
    if not m:
        say(">>> CONTROL FAILED: the bogus command produced no failure text. "
            "Absence and failure are not distinguishable on this console; "
            "aborting.")
        sys.exit(4)
    say(f">>> CONTROL OK: the harness sees a bogus command fail ({m.group(1)!r})")

    cmd = (f"brush /bin/m4-vkwl-a64 {mode} sw {frames} {hold} {presleep} "
           f"{settle} {maxwait}")
    say(f"=== LAUNCH: {cmd} ===")
    launch = time.time()
    ser.send(cmd)

    # 1. Compositor socket. This is the step aarch64/TCG was expected to lose
    #    on, so its own arrival is timed separately from everything after it.
    budget = int(maxwait) + 600
    m, txt = ser.read_until(re.compile(r"M4A: WAYLAND-1 (PRESENT|ABSENT) after (\d+)s"),
                            budget, echo=HEARTBEAT_RE)
    if not m:
        say(">>> STALLED: no WAYLAND-1 verdict at all — the guest driver script "
            "itself did not get that far.")
        print(txt.strip()[-4000:], flush=True)
        sys.exit(2)
    say(f">>> compositor socket: {m.group(1)} after {m.group(2)}s "
        f"(host-measured {time.time() - launch:.0f}s since launch)")
    if m.group(1) == "ABSENT":
        _, txt = ser.read_until(re.compile(r"M4A: DONE"), 300)
        print(txt.strip()[-6000:], flush=True)
        sys.exit(2)

    # 2. Session up, vkwl not started -> control frame.
    m, txt = ser.read_until(re.compile(r"M4A: control window opens now"),
                            int(settle) * 4 + 600, echo=HEARTBEAT_RE)
    if not m:
        say(">>> STALLED: the compositor bound its socket but the driver never "
            "reached the control window.")
        print(txt.strip()[-4000:], flush=True)
        sys.exit(2)
    say(">>> compositor ready; capturing CONTROL A")
    time.sleep(5)
    capA = vnc_capture()
    say(f"    capture A: {capA[0]}x{capA[1]} name={capA[2]!r} "
        f"rects={capA[5]} uncovered={capA[4]}")

    # 3. vkwl parked on each of its last two frames.
    holds = []
    for phase in ("B", "C"):
        m, txt = ser.read_until(HOLD_RE, 3600, echo=HEARTBEAT_RE)
        print(txt.strip()[-5000:], flush=True)
        if not m:
            say(f">>> ABSENT: no 'VKWL: HOLD READY' for phase {phase}.")
            break
        seq, ew, eh, rgb, secs = (int(m.group(1)), int(m.group(2)),
                                  int(m.group(3)), int(m.group(4), 16),
                                  int(m.group(5)))
        say(f">>> HOLD {phase}: seq={seq} extent={ew}x{eh} "
            f"predicted rgb=0x{rgb:06x} for {secs}s; sampling")
        samples, chosen = sample_hold(phase, rgb, 6, 20, max(0, secs - 40))
        holds.append((phase, seq, ew, eh, rgb, samples, chosen))

    # 4. Analysis.
    print("\n\n================ ANALYSIS ================", flush=True)
    print(f"mode={mode} frames={frames} hold={hold} presleep={presleep} "
          f"settle={settle} maxwait={maxwait}")
    wA, hA, _, fbA, missA, _ = capA
    write_ppm(os.path.join(outdir, "capA-control.ppm"), wA, hA, fbA)
    pxA = pixels(wA, hA, fbA)
    print(f"capture A uncovered pixels: {missA}")
    census("A / control (compositor up, no vkwl)", wA, hA, pxA)

    verdicts = []
    for phase, seq, ew, eh, rgb, samples, chosen in holds:
        print(f"\n--- hold {phase} sample series (delay from the "
              f"VKWL: HOLD READY seq={seq} sentinel) ---")
        for k, (dt, cap_k, n_k) in enumerate(samples):
            mark = " <- scored" if k == chosen else ""
            print(f"    #{k + 1} at +{dt:6.1f}s: predicted 0x{rgb:06x} "
                  f"-> {n_k} px{mark}")
        dt, cap, _ = samples[chosen]
        print(f"    scanout carried the predicted colour {dt:.1f}s after the "
              f"sentinel" if samples[chosen][2] else
              "    predicted colour never observed during this hold")
        # Every sample is written out, not just the scored one: a series that
        # starts wrong and ends right is itself the evidence for the latency,
        # and discarding the early frames would hide it.
        for k, (dt_k, cap_k, _) in enumerate(samples):
            write_ppm(os.path.join(
                outdir, f"cap{phase}-seq{seq}-s{k + 1}-t{dt_k:.0f}s.ppm"),
                cap_k[0], cap_k[1], cap_k[3])
        w, h, _, fb, miss, _ = cap
        write_ppm(os.path.join(outdir, f"cap{phase}-seq{seq}.ppm"), w, h, fb)
        px = pixels(w, h, fb)
        print(f"\ncapture {phase} uncovered pixels: {miss}")
        census(f"{phase} / vkwl seq={seq}", w, h, px)
        print(f"  vkwl swapchain extent {ew}x{eh} = {ew * eh} px")
        n, exact, bbox, fill = report_colour(w, h, px, rgb, ew * eh)
        nA, _, _ = find_colour(wA, hA, pxA, rgb)
        print(f"  same colour in CONTROL A: {nA} px")
        swapped = ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)
        nsw, _, _ = find_colour(w, h, px, swapped)
        print(f"  byte-swapped 0x{swapped:06x} (channel-order check): {nsw} px")
        same_as_A = (fb == fbA)
        print(f"  byte-identical to control A: {same_as_A}")
        verdicts.append(dict(phase=phase, seq=seq, rgb=rgb, area=ew * eh,
                             n=n, exact=exact, fill=fill, nA=nA, bbox=bbox,
                             same_as_A=same_as_A, miss=miss))

    same_bbox = None
    if len(holds) == 2:
        fbB = holds[0][5][holds[0][6]][1][3]
        fbC = holds[1][5][holds[1][6]][1][3]
        print(f"\nB and C byte-identical to each other: {fbB == fbC}")
        same_bbox = (verdicts[0]["bbox"] is not None
                     and verdicts[0]["bbox"] == verdicts[1]["bbox"])
        print(f"B and C held colours share one bounding box: {same_bbox} "
              f"({verdicts[0]['bbox']} vs {verdicts[1]['bbox']})")

    print("\n---------------- VERDICT ----------------")
    if missA:
        print("capture A incomplete; treat everything below as provisional.")
    ok = bool(verdicts)
    for v in verdicts:
        crit = {
            "coverage >= 90% of swapchain extent":
                bool(v["area"]) and v["n"] >= 0.90 * v["area"],
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
    else:
        print(f"\n    [{'PASS' if same_bbox else 'FAIL'}] both predicted "
              f"colours land in the SAME bounding box")
        ok = ok and bool(same_bbox)
    print(f"\nOVERALL: {'PASS' if ok else 'FAIL'}")

    ser.read_until(re.compile(r"M4A: DONE"), 900, echo=HEARTBEAT_RE)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
