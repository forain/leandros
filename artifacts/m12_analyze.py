#!/usr/bin/env python3
"""Turn an m12_caps.py run into the capability matrix's evidence.

Reads the run directory produced by m12_caps.py — the frames as PNG and the
whole serial stream as serial.log — and answers, per phase:

  * what the kernel saw   ([DRMSTAT] deltas bracketed by the MARK/ENDMARK lines
                           that sit in the SAME stream, so the window is exact
                           rather than correlated through two clocks);
  * what the screen did   (changed-pixel count and bounding box between the
                           captures taken inside that phase);
  * what the guest said   (the BEGIN/END sections of the exfiltration dump).

The liveness region — the panel clock, the only thing that repaints with nothing
provoked — is measured first and subtracted from every later bounding box.
Without that subtraction every box is dragged up to the panel, and "where is the
window" becomes an answer that cannot be checked against anything.

Field names are read by NAME out of the [DRMSTAT] line, never by position:
c5abb8d once inserted five dmg_* fields mid-line and silently zeroed every
position-keyed parser downstream.

usage: m12_analyze.py <rundir>
"""

import os
import re
import sys

import numpy as np
from PIL import Image

ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[=>78]|\x1b\][^\x07]*\x07")
KV = re.compile(r"([a-z_]+)=0x([0-9A-Fa-f]+)")
FIELDS = ("evpush", "flips_sub", "flips_del", "curs_up", "curs_mv", "atomic",
          "dirtyfb", "dmg_rect", "dmg_full", "dmg_skip", "cplane", "blobs")


def clean(path):
    with open(path, "rb") as f:
        return ANSI.sub("", f.read().decode("utf-8", "replace").replace("\r", ""))


# ------------------------------------------------------------ drm windows ----
def phase_windows(text):
    """(phase, drmstat_at_start, drmstat_at_end) using the interleaving of
    MARK/ENDMARK and [DRMSTAT] within one stream. Both kinds of line come off
    the same serial connection in order, so no clock correlation is needed."""
    cur = None
    last = None
    out = []
    for line in text.splitlines():
        i = line.find("[DRMSTAT]")
        if i >= 0:
            rec = {k: int(v, 16) for k, v in KV.findall(line[i:])}
            if "t" in rec:
                last = rec
                if cur and cur[1] is None:
                    cur[1] = rec        # first sample inside the window
            continue
        m = re.search(r"M12: MARK (\w+) (\d+)", line)
        if m:
            cur = [m.group(1), None, None]
            out.append(cur)
            continue
        m = re.search(r"M12: ENDMARK (\w+)", line)
        if m and cur and cur[0] == m.group(1):
            cur[2] = last
            cur = None
    return out


def drm_report(windows):
    print("\n================ WHAT THE KERNEL SAW ================")
    print(f"{'phase':<12} {'dt':>6}  " +
          "  ".join(f"{f:>10}" for f in FIELDS[:6]))
    rows = []
    for name, a, b in windows:
        if not a or not b:
            print(f"{name:<12} (no bracketing [DRMSTAT] samples — kernel built "
                  f"with DRM_STATS = false?)")
            continue
        dt = (b["t"] - a["t"]) / 100.0
        if dt <= 0:
            print(f"{name:<12} (no tick advance)")
            continue
        d = {f: b.get(f, 0) - a.get(f, 0) for f in FIELDS}
        rows.append((name, dt, d))
        print(f"{name:<12} {dt:6.1f}s  " +
              "  ".join(f"{d[f]:>10}" for f in FIELDS[:6]))
    print(f"\n{'phase':<12} {'dt':>6}  rates per second")
    for name, dt, d in rows:
        print(f"{name:<12} {dt:6.1f}s  evpush {d['evpush'] / dt:8.2f}/s   "
              f"flips {d['flips_sub'] / dt:6.2f}/s   "
              f"atomic {d['atomic'] / dt:6.2f}/s   "
              f"curs_mv {d['curs_mv'] / dt:6.2f}/s   "
              f"curs_up {d['curs_up'] / dt:5.2f}/s")
    return rows


# ---------------------------------------------------------------- frames ----
def load(rundir):
    frames = {}
    for fn in sorted(os.listdir(rundir)):
        if not fn.endswith(".png") or fn.startswith("_"):
            continue
        a = np.asarray(Image.open(os.path.join(rundir, fn)).convert("RGB"))
        frames[fn[:-4]] = (a[:, :, 0].astype(np.uint32) << 16 |
                           a[:, :, 1].astype(np.uint32) << 8 |
                           a[:, :, 2].astype(np.uint32))
    return frames


def dbox(a, b, exclude=None):
    m = a != b
    if exclude:
        x0, y0, x1, y1 = exclude
        m[y0:y1 + 1, x0:x1 + 1] = False
    n = int(m.sum())
    if n == 0:
        return 0, None
    ys, xs = np.nonzero(m)
    return n, (int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max()))


def fmt(n, box, total):
    if n == 0:
        return "IDENTICAL"
    x0, y0, x1, y1 = box
    return (f"{n:>9} px ({100 * n / total:6.3f}%)  x={x0}..{x1} y={y0}..{y1}  "
            f"[{x1 - x0 + 1}x{y1 - y0 + 1}]")


# Which captures are the meaningful before/after pair for each question.
PAIRS = [
    ("compositor is alive at all", "I1_idle_a", "I2_idle_b"),
    ("...and still alive 18 s later", "I2_idle_b", "I3_idle_c"),
    ("pointer parked top-left vs bottom-right", "P1_ptr_tl", "P2_ptr_br"),
    ("pointer parked centre, twice", "P3_ptr_mid", "P4_ptr_mid_again"),
    ("click on the panel applet", "I3_idle_c", "C1_panel_applet_click"),
    ("click far end of the panel", "C1_panel_applet_click", "C2_panel_far_click"),
    ("click the desktop", "C2_panel_far_click", "C3_desktop_click"),
    ("Super (launcher)", "K_key_super_before", "K_key_super_after1"),
    ("Super, 12 s later", "K_key_super_before", "K_key_super_after2"),
    ("Super+/ (launcher)", "K_key_slash_before", "K_key_slash_after1"),
    ("Super+A (app library)", "K_key_a_before", "K_key_a_after1"),
    ("ONE wlclient toplevel mapped", "I3_idle_c", "W1_one_window"),
    ("click on that window", "W1_one_window", "W2_after_click"),
    ("keys 'abc' to the window", "W2_after_click", "W3_after_keys"),
    ("keys 'def' to the window", "W3_after_keys", "W4_after_more_keys"),
    ("SECOND wlclient toplevel mapped", "W5_settle", "X1_two_windows"),
    ("click window 1", "X1_two_windows", "X2_click_win1"),
    ("click window 2", "X2_click_win1", "X3_click_win2"),
    ("keys 'xyz' to the focused window", "X3_click_win2", "X4_keys_to_focused"),
    ("Super+drag to move", "X5_settle", "M1_after_move"),
    ("Super+right-drag to resize", "M1_after_move", "M2_after_resize"),
    ("Super+Q to close", "M2_after_resize", "M3_after_close1"),
    ("Super+Q again", "M3_after_close1", "M4_after_close2"),
    ("cosmic-settings, 6 s after launch", "M5_settle", "S1_settings_early"),
    ("cosmic-settings, 26 s after launch", "M5_settle", "S2_settings_mid"),
    ("cosmic-settings, 74 s after launch", "M5_settle", "S4_settings_late"),
]


def frame_report(frames):
    print("\n================ WHAT THE SCREEN DID ================")
    a, b, c = (frames.get("I1_idle_a"), frames.get("I2_idle_b"),
               frames.get("I3_idle_c"))
    live = None
    if a is not None and b is not None and c is not None:
        boxes = [box for n, box in (dbox(a, b), dbox(b, c)) if box]
        if boxes:
            live = (min(x[0] for x in boxes), min(x[1] for x in boxes),
                    max(x[2] for x in boxes), max(x[3] for x in boxes))
    if live:
        x0, y0, x1, y1 = live
        print(f"liveness region (self-repainting, subtracted below): "
              f"x={x0}..{x1} y={y0}..{y1} [{x1 - x0 + 1}x{y1 - y0 + 1}]\n")
    else:
        print("NO liveness region: nothing repaints on its own. Every "
              "'IDENTICAL' below is then unreadable — a frozen compositor and "
              "an ignored input look exactly the same.\n")

    for label, ka, kb in PAIRS:
        fa, fb = frames.get(ka), frames.get(kb)
        if fa is None or fb is None:
            print(f"{label:<44} (missing {ka if fa is None else kb})")
            continue
        total = fa.size
        n_all, _ = dbox(fa, fb)
        n, box = dbox(fa, fb, live)
        extra = "" if live is None else f"   (raw incl. clock: {n_all} px)"
        print(f"{label:<44} {fmt(n, box, total)}{extra}")


# ------------------------------------------------------------ guest dump ----
def sections(text):
    out = {}
    for m in re.finditer(r"M12: ==== BEGIN (.+?) ====\n(.*?)M12: ==== END \1 ====",
                         text, re.S):
        out[m.group(1)] = m.group(2)
    return out


def main():
    rundir = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m12"
    text = clean(os.path.join(rundir, "serial.log"))
    drm_report(phase_windows(text))
    frame_report(load(rundir))

    sec = sections(text)
    print("\n================ WHAT THE GUEST SAID ================")
    print("sections captured:", ", ".join(sec) or "(none)")
    for k in ("globals.log", "win1.log", "win2.log", "settings.log",
              "pidcensus", "applications", "proc", "runtime"):
        if k in sec:
            print(f"\n---------------- {k} ----------------")
            print(sec[k].strip()[:6000])

    if "bin" in sec:
        names = sorted(set(sec["bin"].split()))
        print(f"\n---------------- /bin ({len(names)} names) ----------------")
        print("  ".join(names))

    log = sec.get("cosmic.log", "")
    if log:
        lines = log.splitlines()
        print(f"\n---------------- cosmic.log: {len(lines)} lines ----------")
        pat = re.compile(r"ERROR|WARN|panic|Panic|error|failed|Failed|refus|"
                         r"No such|not found|unavailable|exited|Exited|"
                         r"libinput|input|seat|udev|Unsupported", re.I)
        hits = [l for l in lines if pat.search(l)]
        print(f"lines matching the error/input filter: {len(hits)}")
        for l in hits:
            print("  " + l[:220])


if __name__ == "__main__":
    main()
