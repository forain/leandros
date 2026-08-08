#!/usr/bin/env python3
"""M19 — confirm on aarch64 what M18 proved only on x86_64/KVM.

M18 root-caused the long-standing false ENOMEM to `ipc::port::LIVE_BUCKETS`
being 64 for the whole system, raised it to 512, and thereby made the busd
`ServiceUnknown` reply landable. Every measurement behind that lives in
artifacts/notes/m18-enomem-port-table-20260808/ and every one of them was taken
on x86_64/KVM. This project mandates both architectures after every change, so
the same session is run here on aarch64/HVF.

WHAT THIS ADDS TO m17_census. The guest half is unchanged (/bin/m17-census) and
so is the analyser, which already matches aarch64's `[EXC] EL0 Fault!` as well
as x86_64's `user page fault`. What is new is a DENSE capture series inside the
last settle window: 22 frames, each hashed over the 220x32 band at the top
centre of the scanout where the COSMIC panel draws its clock. A single frame
cannot distinguish "the panel is drawn" from "the panel is drawn and dead", and
a single frame has produced a geometrically perfect false failure on this
project before. The verdict is the number of DISTINCT hashes over the series.

The mutation is driven from the same file (`M19_TAG` only labels the output),
because control and mutant must differ in the kernel and in nothing else — same
busd binary, same image, same harness, same phase timings.

usage: m19_a64.py <outdir> [arch] [tag]
"""

import hashlib
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import m17_census as m17  # noqa: E402  (path must be set first)

MONITOR_SOCK = "/tmp/leandros-monitor.sock"

# How long to wait for the next phase marker. The healthy session emits its
# first inside 5 s and the whole run is 225 s of phases, so the default is pure
# slack. A run that is EXPECTED to wedge (the falsification) sets this down:
# waiting out a 20-minute default twice proves nothing that 7 minutes does not.
MARK_TIMEOUT = int(os.environ.get("M19_MARK_TIMEOUT", "1200"))

# The COSMIC panel clock: 220 px wide, 32 px tall, centred horizontally at the
# very top of the scanout. Same crop the M9c clock verification used, so a
# FROZEN verdict here is comparable with the one recorded there.
BAND_W, BAND_H = 220, 32


def monitor(cmd, timeout=15):
    """Talk to the QEMU monitor directly. driver.py would do this too, but it
    also shells out to `sips` for a PNG on every shot, which costs more wall
    time than the interval between the frames of a tick series."""
    import socket
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(MONITOR_SOCK)
    time.sleep(0.15)
    try:
        s.recv(65536)
    except Exception:
        pass
    s.sendall((cmd + "\n").encode())
    end = time.time() + timeout
    out = b""
    while time.time() < end:
        try:
            c = s.recv(65536)
        except socket.timeout:
            break
        if not c:
            break
        out += c
        if b"(qemu)" in out[len(cmd):]:
            break
    s.close()
    return out.decode("utf-8", "replace")


def band(path):
    """(sha12, nonzero_bytes, w, h) of the panel-clock band, or None."""
    img = m17.readppm(path) if os.path.exists(path) else None
    if img is None:
        return None
    w, h, px = img
    if w < BAND_W or h < BAND_H:
        return None
    x0 = (w - BAND_W) // 2
    crop = b"".join(px[(y * w + x0) * 3:(y * w + x0 + BAND_W) * 3]
                    for y in range(BAND_H))
    return (hashlib.sha1(crop).hexdigest()[:12], sum(1 for b in crop if b), w, h)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m19"
    arch = sys.argv[2] if len(sys.argv) > 2 else "aarch64"
    tag = sys.argv[3] if len(sys.argv) > 3 else "control"
    os.makedirs(out, exist_ok=True)
    serial_tee = os.path.join(out, "serial.log")
    if os.path.exists(serial_tee):
        os.unlink(serial_tee)
    if os.path.exists(m17.SERIAL_LOG):
        os.unlink(m17.SERIAL_LOG)

    print(f"=== M19 {tag} ({arch}) ===", flush=True)
    r = m17.d("start", arch, t=300)
    print(r.stdout[-600:], r.stderr[-400:], flush=True)
    if "QEMU started" not in r.stdout:
        sys.exit("boot failed")
    r = m17.d("login", "root", "root", t=90)
    print(r.stdout[-400:], flush=True)

    ser = m17.Serial(tee=serial_tee)

    # Absence and failure are indistinguishable on this console unless a command
    # that MUST fail is seen failing first.
    print("\n=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    print(txt.strip()[-300:], flush=True)
    if not m:
        sys.exit(">>> CONTROL DID NOT FAIL — aborting, nothing below is falsifiable")
    print(">>> CONTROL OK\n", flush=True)

    shots = []
    series = []

    def shot(label, dense=False):
        p = os.path.join(out, f"m19-{tag}-{arch}-{label}.ppm")
        monitor(f"screendump {p}")
        img = m17.readppm(p) if os.path.exists(p) else None
        if img is None:
            print(f"  [shot {label}] NO CAPTURE", flush=True)
            return
        w, h, px = img
        ncol, bg, frac = m17.census_px(px)
        prev = shots[-1] if shots else None
        db = m17.diffbox(prev[3], px, w, h) if prev and len(prev[3]) == len(px) else None
        shots.append((label, w, h, px))
        b = band(p)
        if dense:
            series.append(b[0] if b else None)
            print(f"  [tick {label}] band={b[0] if b else 'NONE'} "
                  f"nonzero={b[1] if b else 0} non-bg={frac:.3f}", flush=True)
        else:
            print(f"  [shot {label}] {w}x{h} colours={ncol} bg=#{bg} "
                  f"non-bg={frac:.3f} band={b[0] if b else 'NONE'} "
                  f"diff_vs_prev={db}", flush=True)

    # ONE command for the whole session: serial RX drops characters once a
    # session is live, and a truncated command line has already produced seven
    # EL0 faults that looked exactly like a kernel regression.
    ser.send("brush /bin/m17-census")

    while True:
        m, txt = ser.read_until(m17.MARK, MARK_TIMEOUT)
        print(txt.strip()[-6000:], flush=True)
        if not m:
            print(">>> TIMEOUT waiting for a MARK; the guest stopped early.", flush=True)
            break
        if m.group(0) == "M17: CAPTURES DONE":
            print("\n>>> guest reports CAPTURES DONE", flush=True)
            break
        name, secs = m.group(1), int(m.group(2))
        print(f"\n===== PHASE {name} ({secs}s) =====", flush=True)
        t0 = time.time()
        if name == "SETTLE3":
            # The tick series. 22 frames is the standard the M9c clock lane set;
            # the verdict is how many of the 22 hashes are distinct.
            for i in range(22):
                due = t0 + 1.0 + i * 1.9
                left = due - time.time()
                if left > 0:
                    ser.pump(left)
                shot(f"tick{i:02d}", dense=True)
        else:
            for when in ([4, secs * 0.5, secs - 6] if secs >= 20 else [max(1, secs - 3)]):
                left = when - (time.time() - t0)
                if left > 0:
                    ser.pump(left)
                shot(f"{name.lower()}-t{int(time.time() - t0)}")
        left = secs - (time.time() - t0)
        if left > 0:
            ser.pump(left)

    print(">>> draining guest dump...", flush=True)
    _, txt = ser.read_until(re.compile(r"M17: DONE"), MARK_TIMEOUT)
    print(txt, flush=True)
    ser.pump(5)

    if series:
        good = [h for h in series if h]
        print("\n" + "=" * 72, flush=True)
        print("PANEL CLOCK TICK SERIES (220x32 band, top centre)", flush=True)
        print("=" * 72, flush=True)
        print(f"  frames={len(series)} captured={len(good)} distinct={len(set(good))}",
              flush=True)
        print(f"  VERDICT: {'TICKING' if len(set(good)) > 1 else 'FROZEN/ABSENT'}",
              flush=True)

    m17.analyse(serial_tee)
    m17.d("stop", t=60)


if __name__ == "__main__":
    main()
