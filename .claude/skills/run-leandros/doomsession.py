#!/usr/bin/env python3
"""doomsession.py <arch> <tag> [--gpu virgl|venus] [--scale X] [--no-vt]

DOOM the way a user runs it, with the desktop up. Boots --virgl (the GPU mode
run-qemu.sh picks on the Mac) with guest audio captured to a wav, serial root
login for observation only, then everything the user does goes through the
virtual keyboard:

  desktop   greeter login as leandro, Super+T, `doom` typed in cosmic-term
            (DOOM inherits the session's environment, exactly as a user's
            launch does). DOOM's window must appear and animate, its log must
            say it took the Wayland path, PipeWire must carry its sound, and
            keys sent to the focused window must reach it (Esc opens the menu,
            Enter x3 starts a game).
  vt        (unless --no-vt) Ctrl+Alt+F<n> to a text VT that is not COSMIC's,
            log in there as leandro on the console and type `doom`: it must
            take the display on that VT, play sound and read the keyboard.
            Then back to COSMIC (desktop must present again) and to the text
            VT again.

Every photo is diffed on the host against a reference (changed fraction,
bounding box, colours). Audio: the per-phase RMS of the captured wav.
/proc/vtstat and DOOM's own logs are saved next to the photos.

Output: $DOOMSESSION_OUT (default /tmp/doomsession)/run-<tag>/: *.ppm/*.png,
analysis.json, audio.json, vtstat-*.txt, doom*.log, serial.log, steps.json.
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
WAV = os.path.join(OUT, "audio.wav")
os.environ["LEANDROS_AUDIO_WAV"] = WAV
AUDIO = {}
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
          "&": "shift-7", "_": "shift-minus", "|": "shift-backslash"}


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


def wav_mark():
    try:
        return os.path.getsize(WAV)
    except OSError:
        return 44


def wav_rms(name, start, end):
    """RMS of the captured PCM between two byte offsets (the RIFF header is
    only finalized at exit, so the data is read raw from offset 44). Silence
    still grows the file: only the RMS says something played."""
    import array
    import math
    start = max(44, start - start % 4)
    end = end - end % 4
    with open(WAV, "rb") as f:
        f.seek(start)
        raw = f.read(max(0, end - start))
    a = array.array("h")
    a.frombytes(raw[: len(raw) // 2 * 2])
    n = len(a)
    rms = math.sqrt(sum(x * x for x in a[::8]) / max(1, len(a[::8]))) if n else 0.0
    secs = n / 2 / 44100
    loud = 0
    for k in range(0, n, 88200):
        seg = a[k:k + 88200:8]
        if seg and math.sqrt(sum(x * x for x in seg) / len(seg)) > 100:
            loud += 1
    AUDIO[name] = {"seconds": round(secs, 1), "rms": round(rms, 1), "loud_seconds": loud}
    log(f"audio {name}: {AUDIO[name]}")


def kill_doom():
    sh('for p in /proc/[0-9]*; do c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
       'case "$c" in doom*|/bin/doom*) kill ${p#/proc/};; esac; done; true', 30)


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
    vtstat("before")

    # ── DOOM launched by the user from cosmic-term ────────────────────────
    typ("doom 2>&1 | tee /tmp/doom.log"); key("ret")
    wait_for("doom", lambda: count("doom") >= 1, 60 * SCALE)
    a0 = wav_mark()
    SER.pump(8 * SCALE)
    d0 = shot("desktop-doom-0")
    SER.pump(4 * SCALE)
    d1 = shot("desktop-doom-1")
    wav_rms("desktop-doom", a0, wav_mark())
    analyse("desktop-doom-0", d0, term)
    analyse("desktop-doom-1", d1, term)
    analyse("desktop-doom-animating", d1, d0)
    # Keyboard into the focused window: Esc opens the menu, Enter x3 starts
    # a new game (New Game -> episode -> skill).
    key("esc")
    SER.pump(2 * SCALE)
    km = shot("desktop-doom-menu")
    for _ in range(3):
        key("ret")
        SER.pump(1 * SCALE)
    SER.pump(3 * SCALE)
    kg = shot("desktop-doom-newgame")
    analyse("desktop-doom-menu-vs-prev", km, d1)
    analyse("desktop-doom-newgame-vs-menu", kg, km)
    open(f"{OUT}/doom.log", "w").write(sh("cat /tmp/doom.log", 30))
    vt_doom = vtstat("doom")
    # Quit the desktop DOOM, so whatever plays during the text-VT phase can
    # only be the second DOOM; the gap in between must be silent.
    kill_doom()
    SER.pump(3 * SCALE)
    q0 = wav_mark()
    SER.pump(4 * SCALE)
    wav_rms("after-quit", q0, wav_mark())

    if DO_VT:
        m = re.search(r"^active (\d+)", vt_doom, re.M)
        desk_vt = int(m.group(1)) if m else 1
        tvt = 2 if desk_vt == 1 else 1
        log(f"COSMIC is on VT {desk_vt}; text VT is {tvt}")
        driver.qmp_inject_chord(["ctrl", "alt", f"f{tvt}"])
        SER.pump(3 * SCALE)
        # Console login on the text VT, as the user does. The VT's getty is
        # already at "login:"; an extra Enter here would answer it empty.
        typ("leandro"); key("ret")
        SER.pump(2 * SCALE)
        typ("leandro"); key("ret")
        SER.pump(5 * SCALE)
        tv = shot("textvt")
        vtstat("textvt")
        typ("doom >/tmp/doom2.log 2>&1"); key("ret")
        a1 = wav_mark()
        SER.pump(12 * SCALE)
        tvd = shot("textvt-doom")
        wav_rms("textvt-doom", a1, wav_mark())
        analyse("textvt-doom", tvd, tv)
        key("esc")
        SER.pump(2 * SCALE)
        tvm = shot("textvt-doom-menu")
        analyse("textvt-doom-menu-vs-prev", tvm, tvd)
        vtstat("textvt-doom")
        driver.qmp_inject_chord(["ctrl", "alt", f"f{desk_vt}"])
        SER.pump(5 * SCALE)
        back = shot("desktop-back")
        analyse("desktop-back-vs-textvtdoom", back, tvd)
        vtstat("desktop-back")
        driver.qmp_inject_chord(["ctrl", "alt", f"f{tvt}"])
        SER.pump(5 * SCALE)
        again = shot("textvt-doom-again")
        analyse("textvt-doom-again", again, tv)
        vtstat("textvt-again")
        open(f"{OUT}/doom2.log", "w").write(sh("cat /tmp/doom2.log", 30))
        kill_doom()
        SER.pump(3 * SCALE)
        driver.qmp_inject_chord(["ctrl", "alt", f"f{desk_vt}"])
        SER.pump(4 * SCALE)
        shot("desktop-final")

    SER.close()
    STEPS["total"] = round(time.time() - T0, 1)
    json.dump(STEPS, open(f"{OUT}/steps.json", "w"), indent=1)
    json.dump(RESULT, open(f"{OUT}/analysis.json", "w"), indent=1)
    json.dump(AUDIO, open(f"{OUT}/audio.json", "w"), indent=1)
    subprocess.run(["cp", driver.SERIAL_LOG, f"{OUT}/serial.log"])
    drv("stop", t=120)
    log("DONE", OUT, STEPS)
    log(json.dumps(RESULT, indent=1))
    log(json.dumps(AUDIO, indent=1))


if __name__ == "__main__":
    main()
