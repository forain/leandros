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
# The evdev keyboard module hard-resets the machine every ~10 s inside COSMIC
# (openbugs follow-up); the speed guard runs without it unless --args is given.
EXTRA = opt("--args", "-keyboardprovider none")
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
    }
    return res


def main():
    if "--check" in A:  # mamesession.py - - --check <logfile>: offline check
        res = check(open(opt("--check", "")).read())
        print(json.dumps(res))
        sys.exit(0 if res["mame-wayland"]["pass"] and res["mame-speed"]["pass"] else 1)
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
    ds.typ(f"mame {ROM} -rompath / -str {SECS}{' ' + EXTRA if EXTRA else ''} 2>&1 | tee /tmp/mame.log"); ds.key("ret")
    ds.wait_for("mame-start", lambda: ds.count("mame") >= 1, 60 * ds.SCALE)
    ds.wait_for("mame-exit", lambda: ds.count("mame") == 0, (SECS + 120) * ds.SCALE, every=10)
    text = ds.sh("cat /tmp/mame.log", 30)
    open(f"{OUT}/mame.log", "w").write(text)
    res = check(text)
    res["steps"] = ds.STEPS
    json.dump(res, open(f"{OUT}/analysis.json", "w"), indent=1)
    ds.log("RESULT", json.dumps({k: res[k] for k in ("mame-wayland", "mame-speed", "fps_lines")}))
    ds.drv("stop", t=120)
    sys.exit(0 if res["mame-wayland"]["pass"] and res["mame-speed"]["pass"] else 1)


if __name__ == "__main__":
    main()
