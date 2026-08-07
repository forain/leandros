#!/usr/bin/env python3
"""Host half of liprobe: inject while libinput is listening, with NO compositor.

WHY NO COMPOSITOR. LeandrOS evdev keeps ONE ring per device, not one client
queue per open the way Linux evdev does, so two readers steal each other's
events. cosmic-comp must therefore be absent for this measurement, and its
absence is also the point: this run asks whether *libinput* produces events,
with cosmic-comp, smithay, calloop and libseat all out of the path.

WHAT IT IS FOR. The M13 census localized the break to the window between the
evdev read() and libinput_get_event(): the kernel hands cosmic-comp every queued
event (234 reads, 476 events, 0 ring drops), and cosmic-comp's calloop callback
— which calls schedule_render() for EVERY event it is handed — never runs, since
its flip rate under 476 injected events equals its idle rate exactly. libinput's
own account of what it did with those events is INFO/DEBUG and is discarded at
its default ERROR priority; on top of that cosmic-session never reads
cosmic-comp's stderr pipe, so even the ERROR-level lines are thrown away. The
probe raises the priority and owns the stream, which is the one thing no shipped
component does.

usage: m13b_liprobe.py [outdir]
"""

import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from m12_caps import Serial, Qmp                                  # noqa: E402

RAW_SECS = 8
LI_SECS = 70


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m13b"
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
        print(">>> NO QMP: nothing can be injected, so a zero event count would "
              "be a false negative. Aborting.", flush=True)
        sys.exit(3)

    # /bin/m12c-input is the liprobe ELF for this run (staged over that slot;
    # the original script is backed up beside it). It is an ELF, so it is exec'd
    # directly rather than handed to brush.
    ser.send(f"/bin/m12c-input {RAW_SECS} {LI_SECS} seat0")

    # ---- raw phase: prove the record stream itself, with injection over it ---
    m, txt = ser.read_until(re.compile(r"\[LIP\] RAW begin"), 60)
    print(txt.strip()[-2000:], flush=True)
    if not m:
        print(">>> liprobe never started. Aborting.", flush=True)
        sys.exit(2)
    print("\n===== RAW PHASE: injecting =====", flush=True)
    n, d = q.sweep(RAW_SECS - 2)
    print(f"  motion: {n} moves in {d:.1f}s = {n / d:.1f}/s", flush=True)

    m, txt = ser.read_until(re.compile(r"\[LIP\] RAW verdict[^\n]*"), 60)
    print(txt.strip()[-4000:], flush=True)

    # ---- libinput phase ------------------------------------------------------
    m, txt = ser.read_until(re.compile(r"\[LIP\] libinput_get_fd -> (-?\d+)"), 60)
    print(txt.strip()[-6000:], flush=True)
    if not m:
        print(">>> liprobe never reached libinput_get_fd. Aborting.", flush=True)
        sys.exit(2)
    print(f"\n===== LIBINPUT PHASE: injecting (fd={m.group(1)}) =====", flush=True)

    t0 = time.time()
    n, d = q.sweep(25)
    print(f"  motion: {n} moves in {d:.1f}s = {n / d:.1f}/s", flush=True)
    for (x, y) in ((960, 540), (300, 300)):
        q.move(x, y)
        time.sleep(0.3)
        q.click(x, y)
        time.sleep(0.7)
    print("  two clicks", flush=True)
    for combo in (("meta_l", ()), ("a", ()), ("b", ()), ("esc", ())):
        q.tap(combo[0], combo[1])
        time.sleep(0.6)
    print("  four key taps", flush=True)
    n, d = q.sweep(10)
    print(f"  motion again: {n} moves in {d:.1f}s", flush=True)

    left = LI_SECS - (time.time() - t0)
    if left > 0:
        ser.pump(left + 5)

    print(f"\n[qmp] {q.sent} sent, {q.rejected} rejected", flush=True)
    m, txt = ser.read_until(re.compile(r"\[LIP\] END"), 180)
    print(txt.strip()[-12000:], flush=True)
    if not m:
        print(">>> liprobe never printed END; treat the census as truncated.",
              flush=True)
    print(">>> serial log:", os.path.join(out, "serial.log"), flush=True)


if __name__ == "__main__":
    main()
