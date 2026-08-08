#!/usr/bin/env python3
"""M19 greeter — does a full graphical login still reach a desktop on aarch64?

The greeter path landed hours before the port-table fix (0247506, 8592d91,
69a21c3, debb9a9) and is the headline feature. The busd `ServiceUnknown` reply
now unblocks four more components INSIDE every session that a login starts, so
"the login still works" is not inherited from the greeter lane's own run — it
has to be re-measured with both changes in.

The login is driven the way a person drives it: greetd comes up under
cosmic-comp's kiosk mode, the greeter draws, and the password is typed on the
virtio-keyboard through the QEMU monitor's `sendkey`, which is genuine guest
input (it traverses the same evdev path MAME's keyboard verification used).

WHICH ACCOUNT. cosmic-greeter's UserFilter offers only accounts in
[UID_MIN, UID_MAX) = [1000, 60000), so `root` (0) and `cosmic-greeter` (990)
are deliberately not on the list and `leandro` (1000) is the only choice.

WHAT COUNTS AS REACHING A DESKTOP. Not "the greeter went away" — a dead session
also makes the greeter go away, and the greeter reappearing about a second
later is exactly what a missing /usr/bin/env looked like. The criterion is a
capture holding a drawn panel band whose hash CHANGES between frames, taken
after the greeter is gone and held for the rest of the sample.

usage: m19_greeter.py <outdir> [arch]
"""

import os
import re
import socket
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import m17_census as m17  # noqa: E402
import m19_a64 as m19     # noqa: E402

PASSWORD = "leandro"
# HMP key names. Everything typed here is lowercase ASCII.
KEYMAP = {c: c for c in "abcdefghijklmnopqrstuvwxyz0123456789"}


def sendkey(k):
    m19.monitor(f"sendkey {k}", timeout=8)
    time.sleep(0.25)


def type_text(s):
    for c in s:
        sendkey(KEYMAP.get(c, c))


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m19-greeter"
    arch = sys.argv[2] if len(sys.argv) > 2 else "aarch64"
    os.makedirs(out, exist_ok=True)
    tee = os.path.join(out, "serial.log")
    if os.path.exists(tee):
        os.unlink(tee)

    print(f"=== M19 greeter ({arch}) ===", flush=True)
    r = m17.d("start", arch, t=300)
    print(r.stdout[-500:], r.stderr[-400:], flush=True)
    if "QEMU started" not in r.stdout:
        sys.exit("boot failed")
    print(m17.d("login", "root", "root", t=90).stdout[-300:], flush=True)

    ser = m17.Serial(tee=tee)

    print("\n=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    print(txt.strip()[-250:], flush=True)
    if not m:
        sys.exit(">>> CONTROL DID NOT FAIL — aborting")
    print(">>> CONTROL OK\n", flush=True)

    frames = []

    def shot(label):
        p = os.path.join(out, f"m19-greeter-{arch}-{label}.ppm")
        m19.monitor(f"screendump {p}")
        img = m17.readppm(p) if os.path.exists(p) else None
        if img is None:
            print(f"  [shot {label}] NO CAPTURE", flush=True)
            return None
        w, h, px = img
        ncol, bg, frac = m17.census_px(px)
        b = m19.band(p)
        frames.append((label, b[0] if b else None, frac, ncol))
        print(f"  [shot {label}] {w}x{h} colours={ncol} bg=#{bg} non-bg={frac:.3f} "
              f"band={b[0] if b else 'NONE'}", flush=True)
        return frac

    # ONE command: serial RX drops characters once anything graphical is live.
    ser.send("sh /bin/greeter-real")

    print("\n=== GREETER STARTUP ===", flush=True)
    t0 = time.time()
    for i in range(8):                     # ~40 s for greetd + cosmic-comp + greeter
        ser.pump(5)
        shot(f"greeter-t{int(time.time() - t0)}")
    print(open(tee, "rb").read().decode("utf-8", "replace")[-3000:], flush=True)

    print("\n=== TYPING THE PASSWORD (virtio-keyboard via monitor sendkey) ===",
          flush=True)
    # The password field already holds focus on a single-user list; `ret` first
    # would submit an empty password, so type before submitting. A leading
    # `ret` is sent only if the list needs confirming — cosmic-greeter selects
    # the sole user automatically, so it does not.
    type_text(PASSWORD)
    shot("typed")
    sendkey("ret")

    print("\n=== AFTER LOGIN ===", flush=True)
    t1 = time.time()
    for i in range(20):                    # ~2 min, sampled every ~6 s
        ser.pump(4.5)
        shot(f"post-t{int(time.time() - t1)}")

    ser.pump(5)
    txt = open(tee, "rb").read().decode("utf-8", "replace")
    print(txt[-6000:], flush=True)

    print("\n" + "=" * 72, flush=True)
    print("GREETER VERDICT", flush=True)
    print("=" * 72, flush=True)
    post = [f for f in frames if f[0].startswith("post-")]
    bands = [f[1] for f in post if f[1]]
    print(f"  post-login frames={len(post)} distinct panel bands={len(set(bands))}",
          flush=True)
    print(f"  peak non-bg coverage after login: "
          f"{max((f[2] for f in post), default=0):.3f}", flush=True)
    for pat in ("GREETER-REAL: launching greetd", "Out of memory (os error 12)",
                "[EXC] EL0 Fault!", "port table FULL", "no reply port for this task",
                "cosmic-session", "authentication"):
        print(f"  {pat!r}: {txt.count(pat)}", flush=True)
    m17.analyse(tee)
    m17.d("stop", t=60)


if __name__ == "__main__":
    main()
