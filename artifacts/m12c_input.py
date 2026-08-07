#!/usr/bin/env python3
"""Host half of /bin/m12c-input: inject while the guest listens, then read back.

m12_caps.py established the fact (injected input reaches the kernel's evdev ring
and produces no compositor response whatsoever); this run establishes which
layer drops it. The guest script does the arranging and the exfiltration — see
artifacts/m6-session-data/m12c-input for why each cut is where it is. All this
side does is drive QMP inside the two announced windows and keep the serial
stream in a file.

Injection is deliberately the same shape in both windows — a ~60/s lissajous,
plus clicks and keys in the second — so that "evtest2 saw it" and "the
compositor did not" are statements about the same provocation, not two
different ones.

usage: m12c_input.py [outdir]
"""

import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from m12_caps import Serial, Qmp   # noqa: E402

MARK = re.compile(r"M12C: MARK (\w+) (\d+)|M12C: CAPTURES DONE")


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m12c"
    os.makedirs(out, exist_ok=True)
    ser = Serial(tee=os.path.join(out, "serial.log"))

    print("=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    print(txt.strip()[-300:], flush=True)
    if not m:
        print(">>> CONTROL FAILED: absence and failure are indistinguishable "
              "on this console. Aborting.", flush=True)
        sys.exit(4)
    print(f">>> CONTROL OK ({m.group(1)!r})\n", flush=True)

    q = Qmp(1920, 1080)
    if q.f is None:
        print(">>> NO QMP: nothing can be injected, so every result would be a "
              "false negative. Aborting.", flush=True)
        sys.exit(3)

    ser.send("brush /bin/m12c-input")

    while True:
        m, txt = ser.read_until(MARK, 600)
        print(txt.strip()[-4000:], flush=True)
        if not m:
            print(">>> TIMEOUT waiting for a MARK; the guest stopped early.",
                  flush=True)
            break
        if m.group(0) == "M12C: CAPTURES DONE":
            print("\n>>> guest reports CAPTURES DONE", flush=True)
            break
        name, secs = m.group(1), int(m.group(2))
        print(f"\n===== PHASE {name} ({secs}s) =====", flush=True)
        t0 = time.time()

        if name == "EVTEST":
            # Cover the tool's internal 6 s window wherever inside this one it
            # happens to fall: sweep almost the whole thing.
            n, dur = q.sweep(min(secs - 6, 36))
            print(f"  injected {n} moves in {dur:.1f}s = {n / dur:.1f}/s",
                  flush=True)

        elif name == "POINTER2":
            n, dur = q.sweep(20)
            print(f"  motion: {n} moves in {dur:.1f}s = {n / dur:.1f}/s",
                  flush=True)
            q.click(960, 540)
            q.click(300, 300)
            print("  two clicks", flush=True)
            for combo in (("meta_l", ()), ("slash", ("meta_l",)),
                          ("a", ("meta_l",)), ("esc", ())):
                q.tap(combo[0], combo[1])
                time.sleep(1.0)
            print("  four key combos", flush=True)
            n, dur = q.sweep(12)
            print(f"  motion again: {n} moves in {dur:.1f}s", flush=True)

        left = secs - (time.time() - t0)
        if left > 0:
            ser.pump(left)

    print(f"\n[qmp] {q.sent} sent, {q.rejected} rejected", flush=True)
    print(">>> draining guest dump...", flush=True)
    ser.read_until(re.compile(r"M12C: DONE"), 900)
    print(">>> serial log:", os.path.join(out, "serial.log"), flush=True)


if __name__ == "__main__":
    main()
