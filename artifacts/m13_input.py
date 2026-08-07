#!/usr/bin/env python3
"""Host half of /bin/m12c-input (M13 revision): provoke, then read OUR instruments.

m12_caps established the fact (injected input reaches the kernel's evdev ring and
produces no compositor response at all); m12c_input tried to attribute it and
could not, because the only instrument it had above raw evdev was cosmic-comp's
own log, which has DEBUG compiled out and does not reach the session log file.

This run replaces that missing instrument with three we own and can force to
speak — the libudev shim's call trace, the libseat shim's device-open trace, and
the kernel's per-node [EVSTAT] I/O census — and drives the same provocation past
them. See artifacts/m6-session-data/m12c-input for what each one settles.

Everything measured here is a SERIES, never a point: four windows, two of them
idle, with the idle ones bracketing the provoked ones so that a counter which
climbs for reasons unrelated to the injection is visible as such.

usage: m13_input.py [outdir]
"""

import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import m12_caps as M                                             # noqa: E402
from m12_caps import Serial, Qmp, cap, diff, boxstr              # noqa: E402

MARK = re.compile(r"M13: MARK (\w+) (\d+)|M13: CAPTURES DONE")
KV = re.compile(r"([a-z_]+)=0x([0-9A-Fa-f]+)")


# ------------------------------------------------------------------ [EVSTAT] --
# Parsed by FIELD NAME, never position — the same discipline [DRMSTAT] already
# enforces after c5abb8d silently zeroed every position-keyed parser downstream.
def evstat_last(path):
    """The most recent [EVSTAT] record for each device in the tee'd serial log,
    as {dev: {field: int}}. `depth == 0xffff...` means the kernel's try_lock
    sample was MISSED, not that the ring was empty; it is passed through
    unchanged so a caller cannot mistake one for the other."""
    out = {}
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return out
    for line in data.decode("utf-8", "replace").splitlines():
        i = line.find("[EVSTAT]")
        if i < 0:
            continue
        rec = {k: int(v, 16) for k, v in KV.findall(line[i:])}
        if "dev" in rec and "t" in rec:
            out[rec["dev"]] = rec
    return out


EV = {}     # label -> {dev: rec}


def evsnap(ser, label):
    EV[label] = evstat_last(ser.tee_path)
    return EV[label]


EV_KEYS = ("push", "drop", "depth", "reads", "eagain", "deliv",
           "polls", "pollin", "ioctls", "enotty", "conspop")


def ev_delta(a, b, dev):
    ra, rb = EV.get(a, {}).get(dev), EV.get(b, {}).get(dev)
    if not ra or not rb:
        return f"dev={dev}: [EVSTAT] unavailable (kernel built with EV_STATS = false?)"
    dt = (rb.get("t", 0) - ra.get("t", 0)) / 100.0
    if dt <= 0:
        return f"dev={dev}: no tick advance between {a} and {b}"
    parts = [f"dev={dev}", f"dt={dt:.1f}s"]
    for k in EV_KEYS:
        if k == "depth":
            # An absolute level, not a rate: print both ends.
            parts.append(f"depth {ra.get(k)}->{rb.get(k)}")
        else:
            parts.append(f"{k}+{rb.get(k, 0) - ra.get(k, 0)}")
    parts.append(f"rpid={rb.get('rpid')}")
    parts.append(f"ipid={rb.get('ipid')}")
    parts.append(f"lastnr=0x{rb.get('lastnr', 0):x}")
    return "  ".join(parts)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m13"
    os.makedirs(out, exist_ok=True)
    M.OUT = out
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

    order = []
    while True:
        m, txt = ser.read_until(MARK, 900)
        print(txt.strip()[-6000:], flush=True)
        if not m:
            print(">>> TIMEOUT waiting for a MARK; the guest stopped early.",
                  flush=True)
            break
        if m.group(0) == "M13: CAPTURES DONE":
            print("\n>>> guest reports CAPTURES DONE", flush=True)
            break
        name, secs = m.group(1), int(m.group(2))
        print(f"\n===== PHASE {name} ({secs}s) =====", flush=True)
        t0 = time.time()

        # Snapshot BOTH censuses at the window's opening edge.
        M.snap(ser, name + "_a")
        evsnap(ser, name + "_a")
        order.append(name)

        if name == "POINTER":
            n, d = q.sweep(30)
            print(f"  motion: {n} moves in {d:.1f}s = {n / d:.1f}/s", flush=True)
        elif name == "CLICKKEY":
            for (x, y) in ((960, 540), (300, 300), (960, 16), (40, 1050)):
                q.move(x, y)
                time.sleep(0.3)
                q.click(x, y)
                time.sleep(0.7)
            print("  four clicks (centre, upper-left, panel, dock corner)",
                  flush=True)
            for combo in (("meta_l", ()), ("slash", ("meta_l",)),
                          ("a", ("meta_l",)), ("esc", ()), ("h", ()), ("i", ())):
                q.tap(combo[0], combo[1])
                time.sleep(0.8)
            print("  six key taps", flush=True)

        # Capture inside the window, with margin: a capture that lands after the
        # window closed photographs the NEXT phase.
        left = secs - (time.time() - t0)
        if left > 8:
            ser.pump(left - 8)
        cap(name)

        left = secs - (time.time() - t0)
        if left > 0:
            ser.pump(left)
        M.snap(ser, name + "_b")
        evsnap(ser, name + "_b")

    print(f"\n[qmp] {q.sent} sent, {q.rejected} rejected", flush=True)

    # ---------------------------------------------------------------- report --
    print("\n================ PER-PHASE CENSUS ================", flush=True)
    for name in order:
        print(f"\n-- {name} --", flush=True)
        print("  [DRMSTAT] " + M.drm_delta(name + "_a", name + "_b"), flush=True)
        for dev in (0, 1):
            print("  [EVSTAT]  " + ev_delta(name + "_a", name + "_b", dev),
                  flush=True)

    print("\n================ PIXEL DIFFS ================", flush=True)
    names = [n for n in order if n in M.FRAMES]
    for a, b in zip(names, names[1:]):
        d = diff(M.FRAMES[a], M.FRAMES[b])
        if not d:
            print(f"  {a} -> {b}: frames not comparable", flush=True)
        else:
            print(f"  {a} -> {b}: {d['n']} px changed  box={boxstr(d)}",
                  flush=True)

    print("\n>>> draining guest dump...", flush=True)
    ser.read_until(re.compile(r"M13: DONE"), 900)
    print(">>> serial log:", os.path.join(out, "serial.log"), flush=True)
    print(">>> pngs:", out, flush=True)


if __name__ == "__main__":
    main()
