#!/usr/bin/env python3
"""doomsession.py <arch> <tag> [--gpu virgl|venus] [--scale X] [--no-vt]

Is DOOM visible while COSMIC runs? Boots --virgl (driver's guest RAM), serial
root login, greeter login as leandro, Super+T cosmic-term, then:

  desktop   types `doom -iwad /bin/doom1.wad` in cosmic-term (so DOOM inherits
            the session's WAYLAND_DISPLAY/XDG_RUNTIME_DIR, exactly as a user
            launching it does) and photographs the desktop twice, a few
            seconds apart: DOOM's window must be there and must be animating.
  vt        (unless --no-vt) Ctrl+Alt+F<n> to a text VT that is not COSMIC's, starts a second
            DOOM from the serial root shell (no WAYLAND_DISPLAY there, so it
            takes the KMS path on that VT), photographs it, switches back to
            COSMIC's VT and to the text VT again, photographing each.

Every photo is analysed on the host: the bounding box of the pixels that
changed against the reference shot, and how many distinct colours it holds.
/proc/vtstat and DOOM's own logs are saved next to the photos.

Output: $DOOMSESSION_OUT (default /tmp/doomsession)/run-<tag>/: *.ppm/*.png,
analysis.json, vtstat-*.txt, doom*.log, serial.log, steps.json.
"""
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "doom")
os.environ.setdefault("LEANDROS_VNC_PORT", "5938")
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
os.chdir(REPO)
import driver  # noqa: E402

A = sys.argv
ARCH = A[1]
TAG = A[2]


def opt(name, default, conv=str):
    return conv(A[A.index(name) + 1]) if name in A else default


GPU = opt("--gpu", "virgl")
SCALE = opt("--scale", None, float) or driver.wait_scale(ARCH)
DO_VT = "--no-vt" not in A
OUT = os.path.join(os.environ.get("DOOMSESSION_OUT", "/tmp/doomsession"), f"run-{TAG}")
os.makedirs(OUT, exist_ok=True)
STEPS = {}
RESULT = {}
T0 = time.time()
SER = None


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def key(k):
    driver._monitor_send(f"sendkey {k}")
    time.sleep(0.25)


KEYMAP = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", ">": "shift-dot",
          "&": "shift-7", "_": "shift-minus"}


def typ(s):
    for ch in s:
        key(KEYMAP.get(ch, ch))


def drv(*a, t=400):
    r = subprocess.run([sys.executable, DRV, *a], capture_output=True, text=True, timeout=t)
    log("driver", a[0], "rc", r.returncode, (r.stdout + r.stderr)[-300:])
    return r.returncode


def sh(c, t=60):
    r = driver.serial_run(c, timeout=t * SCALE, arch=ARCH, ser=SER)
    if r["status"] not in ("ok", "rc-file"):
        log(f"sh: {r['status']} for {c[:60]!r}")
    return r["output"]


ARGV = 'cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" "\\n"'


def argv_lines():
    return [l.strip() for l in sh(ARGV, 90).splitlines() if l.strip()]


def have(prefix):
    return lambda: any(os.path.basename(l).startswith(prefix) for l in argv_lines())


def count(name):
    return sum(1 for l in argv_lines() if os.path.basename(l) == name)


def wait_for(name, pred, timeout, every=5.0):
    t = time.time()
    while True:
        try:
            if pred():
                STEPS[name] = round(time.time() - t, 1)
                log(f"{name}: ready after {STEPS[name]}s")
                return True
        except Exception as e:  # noqa: BLE001
            log(f"{name}: poll error {e}")
        if time.time() - t > timeout:
            STEPS[name] = None
            log(f"{name}: NOT ready after {timeout:.0f}s")
            return False
        SER.pump(every)


def read_ppm(path):
    d = open(path, "rb").read()
    parts = d.split(maxsplit=4)
    return int(parts[1]), int(parts[2]), parts[4]


def shot(name):
    path = f"{OUT}/{name}.ppm"
    drv("screenshot", path, t=90)
    return path


def analyse(name, path, ref):
    """Bounding box of the pixels that differ from `ref`, the fraction that
    differ, and the number of distinct colours inside the box."""
    try:
        w, h, p = read_ppm(path)
        wr, hr, pr = read_ppm(ref)
    except Exception as e:  # noqa: BLE001
        RESULT[name] = {"error": str(e)}
        return RESULT[name]
    if (w, h) != (wr, hr):
        RESULT[name] = {"error": f"size {w}x{h} vs ref {wr}x{hr}"}
        return RESULT[name]
    x0, y0, x1, y1, n = w, h, -1, -1, 0
    for y in range(h):
        row = y * w * 3
        a = p[row:row + w * 3]
        b = pr[row:row + w * 3]
        if a == b:
            continue
        for x in range(w):
            i = x * 3
            if a[i:i + 3] != b[i:i + 3]:
                n += 1
                if x < x0: x0 = x
                if x > x1: x1 = x
                if y < y0: y0 = y
                if y > y1: y1 = y
    res = {"size": [w, h], "changed_frac": round(n / (w * h), 4)}
    if n:
        colours = set()
        for y in range(y0, y1 + 1, 2):
            for x in range(x0, x1 + 1, 2):
                i = (y * w + x) * 3
                colours.add(p[i:i + 3])
        res.update(bbox=[x0, y0, x1 - x0 + 1, y1 - y0 + 1], colours=len(colours))
    RESULT[name] = res
    log(f"analyse {name}: {res}")
    return res


def vtstat(tag):
    o = sh("cat /proc/vtstat", 30)
    open(f"{OUT}/vtstat-{tag}.txt", "w").write(o)
    return o


def main():
    global SER
    log(f"doomsession arch={ARCH} accel={driver.accel_kind(ARCH)} gpu={GPU} scale={SCALE}")
    if drv("start", ARCH, f"--{GPU}", t=900) != 0:
        sys.exit("boot failed")
    if drv("login", "root", "root", str(int(60 * SCALE)), t=900) != 0:
        sys.exit("serial login failed")
    SER = driver._Serial()
    log(sh("ls -l /bin/doom /bin/doom1.wad"))

    wait_for("greeter", have("cosmic-greeter"), 120 * SCALE)
    SER.pump(3 * SCALE)
    typ("leandro"); key("ret")
    wait_for("desktop", have("cosmic-panel"), 120 * SCALE)
    SER.pump(5 * SCALE)
    key("meta_l-t")
    wait_for("terminal", have("cosmic-term"), 60 * SCALE)
    SER.pump(4 * SCALE)
    term = shot("term")
    vt_before = vtstat("before")

    # ── DOOM launched by the user from cosmic-term ────────────────────────
    typ("doom -iwad /bin/doom1.wad >/tmp/doom.log 2>&1"); key("ret")
    wait_for("doom", lambda: count("doom") >= 1, 60 * SCALE)
    SER.pump(8 * SCALE)
    d0 = shot("desktop-doom-0")
    SER.pump(4 * SCALE)
    d1 = shot("desktop-doom-1")
    analyse("desktop-doom-0", d0, term)
    analyse("desktop-doom-1", d1, term)
    analyse("desktop-doom-animating", d1, d0)
    open(f"{OUT}/doom.log", "w").write(sh("cat /tmp/doom.log", 30))
    vt_doom = vtstat("doom")

    if DO_VT:
        m = re.search(r"^active (\d+)", vt_doom, re.M)
        desk_vt = int(m.group(1)) if m else 2
        # A text VT other than COSMIC's (VT 1 when COSMIC holds it is a no-op).
        tvt = 2 if desk_vt == 1 else 1
        log(f"COSMIC is on VT {desk_vt}; text VT is {tvt}")
        # ── the text console, and a DOOM started there ────────────────────
        driver.qmp_inject_chord(["ctrl", "alt", f"f{tvt}"])
        SER.pump(3 * SCALE)
        vt1 = shot("textvt")
        vtstat("textvt")
        o = sh("doom -iwad /bin/doom1.wad >/tmp/doom2.log 2>&1 & echo DOOMPID=$!", 30)
        m = re.search(r"DOOMPID=(\d+)", o)
        doom2 = m.group(1) if m else None
        SER.pump(10 * SCALE)
        vt1d = shot("textvt-doom")
        analyse("textvt-doom", vt1d, vt1)
        vtstat("textvt-doom")
        # ── back to the desktop: COSMIC must present again ────────────────
        driver.qmp_inject_chord(["ctrl", "alt", f"f{desk_vt}"])
        SER.pump(5 * SCALE)
        back = shot("desktop-back")
        analyse("desktop-back-vs-doomwin", back, d1)
        analyse("desktop-back-vs-textvtdoom", back, vt1d)
        vtstat("desktop-back")
        # ── and VT 1 again: its DOOM must come back ───────────────────────
        driver.qmp_inject_chord(["ctrl", "alt", f"f{tvt}"])
        SER.pump(5 * SCALE)
        again = shot("textvt-doom-again")
        analyse("textvt-doom-again", again, vt1)
        vtstat("textvt-again")
        open(f"{OUT}/doom2.log", "w").write(sh("cat /tmp/doom2.log", 30))
        if doom2:
            sh(f"kill {doom2}; true", 30)
        SER.pump(3 * SCALE)
        driver.qmp_inject_chord(["ctrl", "alt", f"f{desk_vt}"])
        SER.pump(4 * SCALE)
        shot("desktop-final")

    SER.close()
    STEPS["total"] = round(time.time() - T0, 1)
    json.dump(STEPS, open(f"{OUT}/steps.json", "w"), indent=1)
    json.dump(RESULT, open(f"{OUT}/analysis.json", "w"), indent=1)
    subprocess.run(["cp", driver.SERIAL_LOG, f"{OUT}/serial.log"])
    drv("stop", t=120)
    log("DONE", OUT, STEPS)
    log(json.dumps(RESULT, indent=1))


if __name__ == "__main__":
    main()
