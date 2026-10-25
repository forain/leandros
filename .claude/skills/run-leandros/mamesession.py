#!/usr/bin/env python3
"""mamesession.py <arch> <tag> [--gpu virgl|venus] [--scale X] [--rom captcomm] [--secs 30]

MAME the way a user runs it inside COSMIC: greeter login as leandro, Super+T,
`mame <rom> -rompath / -str <secs>` typed in cosmic-term (MAME inherits the
session's WAYLAND_DISPLAY and opens a window). Waits for MAME to exit, saves
its log and checks it:

  mame-wayland   the log says MAME drew in a Wayland window (not KMS)
  mame-speed     MAME's "Average speed: N%" >= MAME_MIN_SPEED (default 90)

Exit code != 0 when either fails. Output: $MAMESESSION_OUT
(default /tmp/mamesession)/run-<tag>/{mame.log,analysis.json}.
"""
import json
import os
import re
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "mame")
os.environ.setdefault("LEANDROS_VNC_PORT", "5939")
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402
import doomsession as ds  # noqa: E402  (key/typ/sh/wait_for helpers)

A = sys.argv
ARCH, TAG = A[1], A[2]


def opt(name, default, conv=str):
    return conv(A[A.index(name) + 1]) if name in A else default


ROM = opt("--rom", "captcomm")
SECS = opt("--secs", 30, int)
# The keyboard stays ON by default: the evdev reader once took the harness's
# serial bytes (EV_KEY value 2 on event0) as held keys, and MAME hard-reset the
# machine on every serial poll and never reached -str (openbugs L12 #65).
EXTRA = opt("--args", "")
MIN_SPEED = float(os.environ.get("MAME_MIN_SPEED", "90"))
OUT = os.path.join(os.environ.get("MAMESESSION_OUT", "/tmp/mamesession"), f"run-{TAG}")
os.makedirs(OUT, exist_ok=True)


def check(text):
    """Pure log check (unit-testable): returns the analysis dict."""
    m = re.findall(r"Average speed:\s*([0-9.]+)%", text)
    speed = float(m[-1]) if m else None
    res = {
        "mame-wayland": {"pass": "[mame] drawing in a Wayland window" in text},
        "mame-speed": {"speed": speed, "min": MIN_SPEED,
                       "pass": speed is not None and speed >= MIN_SPEED},
        "fps_lines": [l for l in text.splitlines() if l.startswith("[mame] wl fps=")][-4:],
        # One machine, one exit at -str: a hard reset recreates the window and
        # prints another "Average speed" line.
        "mame-single-run": {"runs": len(m),
                            "windows": text.count("[mame] drawing in a Wayland window"),
                            "pass": len(m) == 1 and text.count("[mame] drawing in a Wayland window") == 1},
        # Keys come from the compositor while MAME's window has focus: the
        # session presses "5" (KEY_5 = 6) once MAME runs.
        "mame-wl-key": {"pass": "[mame] key src=wayland code=6 value=1" in text},
        "keys": [l for l in text.splitlines() if l.startswith("[mame] key ")][:12],
    }
    return res


def main():
    if "--check" in A:  # mamesession.py - - --check <logfile>: offline check
        res = check(open(opt("--check", "")).read())
        print(json.dumps(res))
        sys.exit(0 if all(res[k]["pass"] for k in ("mame-wayland", "mame-speed", "mame-single-run", "mame-wl-key")) else 1)
    ds.log(f"mamesession arch={ARCH} gpu={ds.GPU} scale={ds.SCALE} rom={ROM}")
    if ds.drv("start", ARCH, f"--{ds.GPU}", t=900) != 0:
        sys.exit("boot failed")
    if ds.drv("login", "root", "root", str(int(60 * ds.SCALE)), t=900) != 0:
        sys.exit("serial login failed")
    ds.SER = driver._Serial()
    ds.log(ds.sh(f"ls -l /bin/mame /{ROM}.zip"))
    ds.wait_for("greeter", ds.have("cosmic-greeter"), 120 * ds.SCALE)
    ds.SER.pump(3 * ds.SCALE)
    ds.typ("leandro"); ds.key("ret")
    ds.wait_for("desktop", ds.have("cosmic-panel"), 120 * ds.SCALE)
    ds.SER.pump(5 * ds.SCALE)
    ds.key("meta_l-t")
    ds.wait_for("terminal", ds.have("cosmic-term"), 60 * ds.SCALE)
    ds.SER.pump(4 * ds.SCALE)
    ds.typ(f"LEANDROS_MAME_KEYLOG=1 mame {ROM} -rompath / -str {SECS}{' ' + EXTRA if EXTRA else ''} 2>&1 | tee /tmp/mame.log"); ds.key("ret")
    ds.wait_for("mame-start", lambda: ds.count("mame") >= 1, 60 * ds.SCALE)
    ds.SER.pump(8 * ds.SCALE)
    ds.key("5")
    ds.wait_for("mame-exit", lambda: ds.count("mame") == 0, (SECS + 120) * ds.SCALE, every=10)
    text = ds.sh("cat /tmp/mame.log", 30)
    open(f"{OUT}/mame.log", "w").write(text)
    res = check(text)
    res["steps"] = ds.STEPS
    res["mame-single-run"]["pass"] &= ds.STEPS.get("mame-exit") is not None
    json.dump(res, open(f"{OUT}/analysis.json", "w"), indent=1)
    ds.log("RESULT", json.dumps({k: res[k] for k in ("mame-wayland", "mame-speed", "mame-single-run", "mame-wl-key", "keys")}))
    ds.drv("stop", t=120)
    sys.exit(0 if all(res[k]["pass"] for k in ("mame-wayland", "mame-speed", "mame-single-run", "mame-wl-key")) else 1)


if __name__ == "__main__":
    main()
