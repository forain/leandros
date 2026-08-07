#!/usr/bin/env python3
"""Host half of /bin/m14-input: inject input, then read it back on BOTH sides of
the compositor.

M13 exonerated everything below and including libinput by measurement, in
process: the libudev shim enumerates and resolves, the libseat shim opens
event0/event1, the kernel's evdev census shows reads and POLLINs, and libinput
itself produced motion_abs=62 key=8 with dispatch_err=0. cosmic-comp acts on
none of it, and cosmic-comp cannot be asked why — it must not be patched, its
logger pins smithay=warn through add_directive (which RUST_LOG cannot override)
and writes to a stdout nobody captures.

So this run brackets it. Below: the kernel's per-node [EVSTAT] census, which
depends on no userspace component agreeing to log. Above: /bin/wlinput, a
Wayland client with a mapped xdg_toplevel that counts every wl_pointer /
wl_keyboard / wl_touch event cosmic-comp sends it. One injection, two censuses,
and the gap between them is the compositor.

  evdev climbs, client climbs  -> input reaches clients; the failure is
                                  cursor/render-side, not routing.
  evdev climbs, client flat    -> cosmic-comp is dropping them.
  evdev flat                   -> the injection did not arrive; nothing else in
                                  the run means anything.

Every counter is read as a SERIES over four windows, idle at both ends, so a
counter climbing for reasons unrelated to the injection is visible as such.

usage: m14_input.py [outdir]
"""

import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import m12_caps as M                                             # noqa: E402
from m12_caps import Serial, Qmp, cap, diff, boxstr              # noqa: E402
from m13_input import evstat_last, EV_KEYS                       # noqa: E402

MARK = re.compile(r"M14: MARK (\w+) (\d+)|M14: CAPTURES DONE")

# ------------------------------------------------------------------- [WLI] --
# The client's census line, parsed by FIELD NAME like every other census here.
# `[WLI] CENSUS t=12.0s ptr_enter=0 ptr_motion=0 ...`
WLI_KV = re.compile(r"([a-z_0-9]+)=(\d+)")


def wli_last(path):
    """The most recent [WLI] CENSUS record in the tee'd serial log, plus the
    control flags. Returns (counters, controls) where controls records whether
    the client ever got as far as being a surface input could be routed to.

    A missing CENSUS is NOT zero: it means the client never ran or its output
    never arrived, and the caller must be able to tell that apart from a client
    that ran and counted nothing. `None` is returned for the former."""
    rec, ctl = None, {"begin": 0, "bound": 0, "seatcap": None,
                      "configure": 0, "mapped": 0, "fail": []}
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return None, ctl
    for line in data.decode("utf-8", "replace").splitlines():
        i = line.find("[WLI]")
        if i < 0:
            continue
        s = line[i:]
        if " CENSUS " in s:
            rec = {k: int(v) for k, v in WLI_KV.findall(s.split("CENSUS", 1)[1])}
        elif " BEGIN " in s:
            ctl["begin"] += 1
        elif " BOUND " in s:
            ctl["bound"] += 1
        elif " SEATCAP " in s:
            ctl["seatcap"] = s
        elif " CONFIGURE " in s:
            ctl["configure"] += 1
        elif " MAPPED " in s:
            ctl["mapped"] += 1
        elif " FAIL " in s or " NOCONFIGURE " in s:
            ctl["fail"].append(s)
    return rec, ctl


WLI = {}    # label -> counters
EV = {}     # label -> {dev: rec}


def snapshot(ser, label):
    EV[label] = evstat_last(ser.tee_path)
    WLI[label], _ = wli_last(ser.tee_path)


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
            parts.append(f"depth {ra.get(k)}->{rb.get(k)}")
        else:
            parts.append(f"{k}+{rb.get(k, 0) - ra.get(k, 0)}")
    parts.append(f"rpid={rb.get('rpid')}")
    return "  ".join(parts)


# Only the fields that answer the question; the client prints more.
WLI_KEYS = ("ptr_enter", "ptr_leave", "ptr_motion", "ptr_button", "ptr_frame",
            "kbd_keymap", "kbd_enter", "kbd_leave", "kbd_key", "kbd_mods",
            "tch_down", "tch_motion", "configure", "frame_cb")


def wli_delta(a, b):
    ra, rb = WLI.get(a), WLI.get(b)
    if ra is None or rb is None:
        return ("[WLI] no CENSUS line yet — the client had not printed one at "
                "this window's edge. NOT the same as a count of zero.")
    return "  ".join(f"{k}+{rb.get(k, 0) - ra.get(k, 0)}" for k in WLI_KEYS)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m14"
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

    ser.send("brush /bin/m14-input")

    order = []
    while True:
        m, txt = ser.read_until(MARK, 900)
        print(txt.strip()[-8000:], flush=True)
        if not m:
            print(">>> TIMEOUT waiting for a MARK; the guest stopped early.",
                  flush=True)
            break
        if m.group(0) == "M14: CAPTURES DONE":
            print("\n>>> guest reports CAPTURES DONE", flush=True)
            break
        name, secs = m.group(1), int(m.group(2))
        print(f"\n===== PHASE {name} ({secs}s) =====", flush=True)
        t0 = time.time()

        snapshot(ser, name + "_a")
        order.append(name)

        if name == "POINTER":
            n, d = q.sweep(30)
            print(f"  motion: {n} moves in {d:.1f}s = {n / d:.1f}/s", flush=True)
        elif name == "CLICKKEY":
            # Centre first and dwell there: the client's toplevel is the thing
            # under test, and a click that lands on the panel or the dock tests
            # cosmic-comp's own surfaces instead.
            for (x, y) in ((960, 540), (900, 500), (1000, 600), (960, 540)):
                q.move(x, y)
                time.sleep(0.4)
                q.click(x, y)
                time.sleep(0.8)
            print("  four clicks, all inside the client's toplevel", flush=True)
            for combo in (("a", ()), ("b", ()), ("h", ()), ("i", ()),
                          ("esc", ()), ("meta_l", ())):
                q.tap(combo[0], combo[1])
                time.sleep(0.8)
            print("  six key taps", flush=True)

        left = secs - (time.time() - t0)
        if left > 8:
            ser.pump(left - 8)
        cap(name)

        left = secs - (time.time() - t0)
        if left > 0:
            ser.pump(left)
        snapshot(ser, name + "_b")

    print(f"\n[qmp] {q.sent} sent, {q.rejected} rejected", flush=True)

    # ---------------------------------------------------------------- report --
    _, ctl = wli_last(ser.tee_path)
    print("\n================ CLIENT CONTROLS ================", flush=True)
    print(f"  BEGIN={ctl['begin']}  BOUND={ctl['bound']}  "
          f"CONFIGURE={ctl['configure']}  MAPPED={ctl['mapped']}", flush=True)
    print(f"  {ctl['seatcap']}", flush=True)
    for f in ctl["fail"]:
        print(f"  !! {f}", flush=True)
    if not (ctl["configure"] and ctl["mapped"]):
        print("  >>> BROKEN RUN: without CONFIGURE and MAPPED the client was "
              "never a surface input could be routed to, so a zero input count "
              "below says NOTHING about the compositor.", flush=True)

    print("\n================ PER-PHASE CENSUS ================", flush=True)
    for name in order:
        print(f"\n-- {name} --", flush=True)
        print("  [DRMSTAT] " + M.drm_delta(name + "_a", name + "_b"), flush=True)
        for dev in (0, 1):
            print("  [EVSTAT]  " + ev_delta(name + "_a", name + "_b", dev),
                  flush=True)
        print("  [WLI]     " + wli_delta(name + "_a", name + "_b"), flush=True)

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
    ser.read_until(re.compile(r"M14: DONE"), 900)
    print(">>> serial log:", os.path.join(out, "serial.log"), flush=True)
    print(">>> pngs:", out, flush=True)


if __name__ == "__main__":
    main()
