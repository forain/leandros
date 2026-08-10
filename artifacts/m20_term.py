#!/usr/bin/env python3
# m20_term.py — host half of artifacts/m6-session-data/m20-term.
#
# Boots, logs in, runs the guest driver with a persistent serial drainer, and
# photographs the framebuffer while cosmic-term is up. Then types into the
# window through the emulated keyboard so the picture can show a command and its
# output, not just a prompt.
#
# SHOT TIMES ARE RELATIVE TO A MARKER, NOT TO BOOT. The guest reaches
# `M20: MARK TERM` only after ptytest, the session handshake and a 25 s settle,
# and those costs vary by tens of seconds between arches and between a cold and
# a warm image. Fixed offsets from t0 would therefore photograph a different
# phase on every run, and a blank frame would be unattributable. This waits for
# the marker in the serial log and times everything from there.
#
# --venus IS DELIBERATELY NOT USED. driver.py's own docstring records that
# screendump cannot photograph a Venus session on any device= argument:
# virgl_cmd_set_scanout() leaves console->scanout.kind = SCANOUT_TEXTURE and
# qemu_console_surface() returns NULL for anything but SCANOUT_SURFACE. The
# COSMIC session is software-rendered by construction anyway
# (GBM_ALWAYS_SOFTWARE=1, ICED_BACKEND=tiny-skia), so Venus would buy nothing
# here and would cost the photograph, which is the deliverable.
#
# EVERY SCREENSHOT REPAINTS THE FRAMEBUFFER CONSOLE, WHICH IS THE SCANOUT. A
# single frame cannot separate "the pixels never arrive" from "they had not
# arrived yet", and the first frame of any run is suspect. Hence several shots
# spread across the window, kept individually rather than reduced to one.

import os
import re
import subprocess
import sys
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DRIVER = os.path.join(REPO, ".claude", "skills", "run-leandros", "driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"

ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "r1"
TYPED = sys.argv[3] if len(sys.argv) > 3 else "id"
OUT = os.path.expanduser(f"~/m20-shots/{ARCH}-{TAG}")

# QEMU's `sendkey` takes qcodes, not characters, so anything typed has to be
# spelled out. Only what these runs actually need is mapped; an unmapped
# character is a hard error rather than a silently dropped keystroke, because a
# command that types itself incompletely looks exactly like a shell that ignored
# the input.
QCODE = {" ": "spc", "-": "minus", ".": "dot", "/": "slash"}


def qcodes_for(text):
    keys = []
    for ch in text:
        if ch.isalnum():
            keys.append(ch)
        elif ch in QCODE:
            keys.append(QCODE[ch])
        else:
            raise SystemExit(f"ERROR: no qcode mapped for {ch!r} in {text!r}")
    return keys + ["ret"]

# aarch64 on an x86_64 host is TCG, so every phase costs several times what it
# does under KVM. Scaling the whole schedule by one factor keeps the shots in
# the same *phases* on both arches instead of the same wall-clock seconds, which
# is what makes the two runs comparable.
SLOW = ARCH == "aarch64"
K = 3 if SLOW else 1

# The guest sleeps PRE after launching cosmic-term, then POST more. Shots
# bracket the typing so a change in the picture is attributable to the
# keystrokes rather than to time passing.
PRE, POST = (150, 180) if SLOW else (45, 60)
SHOTS_BEFORE = [12 * K, 25 * K, 40 * K]
TYPE_AT = 48 * K
SHOTS_AFTER = [58 * K, 70 * K, 85 * K, 100 * K]

DRAIN = 420 * K
MARKER_TIMEOUT = 300 * K


def d(*a, t=260):
    try:
        r = subprocess.run([sys.executable, DRIVER, *a],
                           capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def log(*a):
    print(*a, flush=True)


def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)


def monitor_send(cmd):
    """One connect-send-close per command. The QEMU monitor serves a single
    client at a time and driver.py's screenshot needs it too, so the socket is
    never held open between calls."""
    sys.path.insert(0, os.path.dirname(DRIVER))
    import importlib.util
    spec = importlib.util.spec_from_file_location("leandros_driver", DRIVER)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod._monitor_send(cmd, timeout=10)


def wait_for_marker(marker, timeout):
    """Poll the serial log the drainer is filling. There is no back-channel from
    the guest, so the log is the only signal that a phase has started."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with open(SERIAL_LOG, "r", errors="replace") as f:
                if marker in f.read():
                    return time.time() - t0
        except OSError:
            pass
        time.sleep(1)
    return None


def shot(when, note=""):
    ppm = f"{OUT}/m20-{ARCH}-{TAG}-t{when}.ppm"
    d("screenshot", ppm, t=45)
    sz = os.path.getsize(ppm) if os.path.exists(ppm) else 0
    log(f"[shot +{when}s -> {os.path.basename(ppm)} ({sz} B)] {note}")
    return ppm


def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M20 cosmic-term first light: {ARCH} tag={TAG} {time.ctime()} ====")
    try:
        os.remove(SERIAL_LOG)
    except OSError:
        pass

    clean()
    # 4G, not the 2G default: a COSMIC session plus a terminal is the workload
    # the driver's own docstring raises this for.
    os.environ["LEANDROS_QEMU_MEM"] = "4G"
    out = d("start", ARCH, t=300)
    if not any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
        log("no boot"); log(out[-2000:]); clean(); return
    log("[booted]")
    d("login", "root", "root", t=60)
    log("[logged in]")

    def drainer():
        d("session", str(DRAIN), f"brush /bin/m20-term {PRE} {POST}", t=DRAIN + 60)

    th = threading.Thread(target=drainer, daemon=True)
    th.start()
    log(f"[guest driver launched; PRE={PRE} POST={POST}, draining {DRAIN}s]")

    waited = wait_for_marker("M20: MARK TERM", timeout=MARKER_TIMEOUT)
    if waited is None:
        log("!! never reached MARK TERM — cosmic-term was never launched")
    else:
        log(f"[MARK TERM at t+{waited:.0f}s; timing shots from here]")
        t0 = time.time()

        for when in SHOTS_BEFORE:
            dt = when - (time.time() - t0)
            if dt > 0:
                time.sleep(dt)
            shot(when, "pre-input")

        dt = TYPE_AT - (time.time() - t0)
        if dt > 0:
            time.sleep(dt)
        log(f"[typing {TYPED!r} + Enter through virtio-keyboard]")
        for key in qcodes_for(TYPED):
            r = monitor_send(f"sendkey {key}")
            log(f"  sendkey {key} -> {r.strip()[:60]}")
            time.sleep(0.4)

        for when in SHOTS_AFTER:
            dt = when - (time.time() - t0)
            if dt > 0:
                time.sleep(dt)
            shot(when, "post-input")

    th.join(timeout=DRAIN + 70)

    try:
        with open(SERIAL_LOG, "r", errors="replace") as f:
            data = f.read()
        clean_txt = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "",
                           re.sub(r"\x1b[=>78]", "", data))
        open(f"{OUT}/serial.txt", "w").write(clean_txt[-1500000:])
        log(f"[serial -> {OUT}/serial.txt ({len(clean_txt)} B)]")
        for key in ("M20:", "ptytest", "PASS", "FAIL", "panic", "assertion",
                    "TIOCSCTTY", "TIOCSWINSZ", "ptmx", "pts/",
                    "Noto Sans Mono", "wayland-1", "EL0 Fault",
                    "failed to daemonize", "brush-0.5"):
            n = clean_txt.count(key)
            if n:
                log(f"  serial: '{key}' x{n}")
    except Exception as e:
        log(f"[serial err] {e}")

    clean()
    log("==== M20 DONE ====")


if __name__ == "__main__":
    main()
