#!/usr/bin/env python3
# m21_cpr.py — the decisive run for TODO item 17: is reedline's CPR-driven
# repaint the eraser, and does the crossterm fork actually fix it?
#
# Host half; reuses artifacts/m6-session-data/m20-term as the guest half (it
# already brings up a COSMIC session and launches cosmic-term with its stderr
# captured, which is all this needs).
#
# WHY THIS IS ONE BOOT AND NOT THREE. Item 17's fix lives in a sibling repo
# wired in through brush's `[patch.crates-io]`, so a fixed brush and a broken
# brush cannot both exist unless both are BUILT and STAGED. They are:
# /bin/brush (patched crossterm) and /bin/brush-nofix (stock crossterm 0.29.0),
# same source, same flags, same image. Running them back to back inside ONE
# cosmic-term window is what makes "output is visible now" a measurement rather
# than a recollection — across runs the difference could be attributed to the
# arch, the image, TCG timing or emulator state, and item 17's whole history is
# a history of exactly those misattributions.
#
# THE THIRD SHELL IS THE CONTROL AND IT IS THE POINT. `brush --input-backend=
# basic` (brush-shell/src/args.rs:203, entry.rs:285) never calls
# cursor::position() and never repositions the cursor. If its output is visible
# while reedline's is not, reedline's repaint is conclusively the eraser. If its
# output is ALSO invisible, the root cause recorded in item 17 is wrong and the
# crossterm fork is treating a symptom — that outcome is worth more than a
# passing fix, so it is measured first in importance even though it is typed
# last (the shells must be entered and exited in a safe order).
#
# SHOT TIMES ARE RELATIVE TO A MARKER, NOT TO BOOT — inherited from m20_term.py
# for the same reason: the guest reaches `M20: MARK TERM` only after ptytest,
# the session handshake and a 25 s settle, and those costs vary by tens of
# seconds between arches and between a cold and a warm image.
#
# TWO SHOTS AT EVERY MEASUREMENT POINT. A single frame cannot separate "the
# output never arrived" from "it had not arrived yet", which is the precise
# distinction this run exists to make, and a false "invisible" would re-open a
# closed root cause.

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
# MODE matters more than it looks. On an Apple Silicon host `start aarch64`
# means `-cpu host -accel hvf`, but item 17's aarch64 failure was recorded on
# the Linux box, where an aarch64 guest is TCG — and the recorded mechanism is
# that TCG widens the CPR orphan window ~30x. So the Mac's DEFAULT aarch64 is
# the configuration least likely to reproduce the bug, and "it works here"
# under HVF would prove nothing about the fix. `uefi-tcg` is the faithful
# reproduction; both are worth running, and they must be labelled apart.
MODE = sys.argv[3] if len(sys.argv) > 3 else "uefi"
OUT = os.path.expanduser(f"~/m21-shots/{ARCH}-{TAG}")

# QEMU's `sendkey` takes qcodes, not characters. An unmapped character is a hard
# error rather than a silently dropped keystroke, because a command that types
# itself incompletely looks exactly like a shell that ignored the input.
QCODE = {" ": "spc", "-": "minus", ".": "dot", "/": "slash", "=": "equal"}


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


# aarch64 on this host is TCG. Scaling the schedule by one factor keeps the
# shots in the same *phases* on both arches rather than the same wall-clock
# seconds, which is what makes the two runs comparable.
# One scale factor for the whole schedule keeps the shots in the same *phases*
# across accelerators instead of the same wall-clock seconds.
HVF = ARCH == "aarch64" and MODE in ("uefi", "uefi-hvf") and sys.platform == "darwin"
if HVF:
    K = 1.0          # hardware virtualisation
elif ARCH == "x86_64":
    K = 1.5          # TCG, but a native-width guest
else:
    K = 5.0          # aarch64 under TCG — the slow, bug-reproducing case

DRAW_WAIT = int(600 * K)          # CAP on waiting for the window, not a delay
SETTLE_AFTER_WINDOW = int(25 * K)
STEP_WAIT = int(16 * K)           # a command runs and prints
LAUNCH_WAIT = int(24 * K)         # a nested shell starts and prompts
# The guest half must stay alive across the whole typed sequence, and the
# sequence no longer starts at a time known in advance — so POST is sized for
# the worst case rather than the expected one. Sleeping too long costs only
# wall-clock; ending early costs the run.
PRE = int(40 * K)
POST = int(900 * K)
DRAIN = int(1400 * K)
MARKER_TIMEOUT = int(240 * K)

# (typed text, settle seconds, shot label, how many shots)
#
# Order is forced by safety, not by importance: the nested shells have to be
# entered and left one at a time, and `exit` must return to a known prompt
# before the next one is launched. The control (`basic`) is last because it is
# the only step whose failure invalidates everything above it, so it must not
# be able to disturb the steps it would invalidate.
# Absolute paths, not bare names: nothing in the guest half exports PATH, and a
# `command not found` would print nothing where the bug also prints nothing.
#
# THE SHELLS NEST AND ARE NEVER EXITED. An `exit` between them is what killed
# the first attempt: the nested shell had not actually started (its keystrokes
# were typed before the window existed), so `exit` reached the LOGIN shell
# instead and cosmic-term closed with three measurements still untaken.
# Nesting one level deeper costs nothing and has no such failure mode.
#
# NO `PS1=` STEP IS NEEDED — brush already identifies itself. Its default PS1
# is `\s-\v\$`, and `\s` is argv[0]'s basename, so the two binaries prompt as
# `brush-0.5#` and `brush-nofix-0.5#` on their own. An explicit PS1 assignment
# was tried and is strictly worse: it is 10 more keystrokes on an input path
# that demonstrably DROPS them (a typed `PS1=nofix.` arrived as `1=nofix.`),
# so it added a way for the witness itself to fail.
STEPS = [
    ("id",                               STEP_WAIT,   "A-patched-id",        2),
    ("/bin/brush-nofix",                 LAUNCH_WAIT, "B-nofix-launched",    1),
    ("id",                               STEP_WAIT,   "C-nofix-id",          2),
    ("/bin/brush --input-backend=basic", LAUNCH_WAIT, "D-basic-launched",    1),
    ("id",                               STEP_WAIT,   "E-basic-id-CONTROL",  2),
]


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


_drv_mod = None


def monitor_send(cmd):
    """One connect-send-close per command. The QEMU monitor serves a single
    client at a time and driver.py's screenshot needs it too, so the socket is
    never held open between calls."""
    global _drv_mod
    if _drv_mod is None:
        import importlib.util
        spec = importlib.util.spec_from_file_location("leandros_driver", DRIVER)
        _drv_mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(_drv_mod)
    return _drv_mod._monitor_send(cmd, timeout=10)


def wait_for_marker(marker, timeout):
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


def ppm_mean_luma(path, x0=0.30, y0=0.30, x1=0.70, y1=0.70):
    """Mean luminance of a central box of a binary (P6) PPM.

    Used to detect the cosmic-term window. The wallpaper under it is the bright
    core of the Orion Nebula; the terminal is near-black. Nothing else on this
    desktop covers the middle of the screen, so a large sustained drop here is
    the window opening and nothing else.
    """
    with open(path, "rb") as f:
        data = f.read()
    # P6 header: magic, width, height, maxval — whitespace separated, '#' to EOL
    # is a comment. Parsed rather than assumed because a short read would
    # otherwise be silently misinterpreted as a very dark frame.
    pos, toks = 0, []
    while len(toks) < 4:
        while pos < len(data) and data[pos:pos + 1].isspace():
            pos += 1
        if data[pos:pos + 1] == b"#":
            while pos < len(data) and data[pos] != 0x0A:
                pos += 1
            continue
        start = pos
        while pos < len(data) and not data[pos:pos + 1].isspace():
            pos += 1
        toks.append(data[start:pos])
    pos += 1
    if toks[0] != b"P6":
        raise ValueError(f"not a P6 PPM: {toks[0]!r}")
    w, h = int(toks[1]), int(toks[2])
    if len(data) - pos < w * h * 3:
        raise ValueError("truncated PPM")
    total = n = 0
    # Stride the sample: a full 1920x1080 scan in pure Python costs seconds per
    # frame and this runs in a poll loop.
    for y in range(int(h * y0), int(h * y1), 4):
        row = pos + y * w * 3
        for x in range(int(w * x0), int(w * x1), 4):
            o = row + x * 3
            total += data[o] + data[o + 1] + data[o + 2]
            n += 1
    return total / (3.0 * n) if n else 0.0


def wait_for_window(timeout, poll=20):
    """Poll the framebuffer until the cosmic-term window is actually on screen.

    A FIXED draw delay is what made the first x86_64 attempt useless: at +45 s
    the desktop had only the wallpaper, so every keystroke of the first step was
    typed into no window at all and was simply lost — and a lost keystroke
    prints nothing, which is indistinguishable from the erasure bug being
    measured. The window has to be OBSERVED before anything is typed, or a
    harness failure gets recorded as a guest result.

    Detection is TWO-PHASE, and it has to be. The screen goes black → wallpaper
    → terminal, so "the centre is dark" is true both before the desktop exists
    and after the window opens. A first attempt anchored on the first sample
    and was defeated by sampling during the black phase: baseline 0.0 made
    every later frame "not darker than baseline", so the window could never be
    detected at all. Wait for the wallpaper to come up first (a RISING edge),
    and only then treat darkness as the window.
    """
    probe = f"{OUT}/_probe.ppm"
    t0 = time.time()
    desktop_up = False
    while time.time() - t0 < timeout:
        d("screenshot", probe, t=60)
        try:
            luma = ppm_mean_luma(probe)
        except Exception as e:
            log(f"  [probe unreadable: {e}]")
            time.sleep(poll)
            continue
        el = time.time() - t0
        if not desktop_up:
            log(f"  [probe t+{el:.0f}s luma={luma:.1f} (waiting for wallpaper)]")
            if luma > 80:
                desktop_up = True
                log(f"  [DESKTOP UP after {el:.0f}s (luma={luma:.1f})]")
        else:
            log(f"  [probe t+{el:.0f}s luma={luma:.1f} (waiting for window)]")
            if luma < 70:
                log(f"  [WINDOW DETECTED after {el:.0f}s]")
                return el
        time.sleep(poll)
    log(f"  !! no window detected within {timeout}s — typing anyway, results suspect")
    return None


def shot(label, n=1):
    paths = []
    for i in range(n):
        if i:
            time.sleep(7)
        ppm = f"{OUT}/{ARCH}-{TAG}-{label}-{i}.ppm"
        d("screenshot", ppm, t=60)
        sz = os.path.getsize(ppm) if os.path.exists(ppm) else 0
        log(f"  [shot {label}#{i} -> {os.path.basename(ppm)} ({sz} B)]")
        paths.append(ppm)
    return paths


def type_line(text):
    # KEYS ARE DROPPED IF TYPED TOO FAST, and not by QEMU. A typed
    # `PS1=nofix.` arrived at the shell as `1=nofix.` at 0.35 s/key — the
    # leading two characters simply never made it, while everything after did.
    # That is silent, partial, and produces a command that looks like something
    # the guest chose to do. Spend the wall-clock instead.
    delay = 0.35 + 0.45 * K
    log(f"  [typing {text!r} + Enter @ {delay:.2f}s/key]")
    for key in qcodes_for(text):
        monitor_send(f"sendkey {key}")
        time.sleep(delay)


def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M21 CPR/reedline eraser test: {ARCH} mode={MODE} "
        f"(K={K}) tag={TAG} {time.ctime()} ====")
    try:
        os.remove(SERIAL_LOG)
    except OSError:
        pass

    clean()
    os.environ["LEANDROS_QEMU_MEM"] = "4G"
    out = d("start", ARCH, MODE, t=int(300 * K))
    if not any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
        log("no boot"); log(out[-2000:]); clean(); return
    log("[booted]")
    d("login", "root", "root", t=90)
    log("[logged in]")

    def drainer():
        d("session", str(DRAIN), f"brush /bin/m20-term {PRE} {POST}", t=DRAIN + 90)

    th = threading.Thread(target=drainer, daemon=True)
    th.start()
    log(f"[guest driver launched; PRE={PRE} POST={POST}, draining {DRAIN}s]")

    waited = wait_for_marker("M20: MARK TERM", timeout=MARKER_TIMEOUT)
    if waited is None:
        log("!! never reached MARK TERM — cosmic-term was never launched")
    else:
        log(f"[MARK TERM at t+{waited:.0f}s; waiting for the window to actually appear]")
        wait_for_window(DRAW_WAIT)
        # Even once the window exists, its shell has to reach a prompt before a
        # command can be typed at it.
        time.sleep(SETTLE_AFTER_WINDOW)
        shot("0-drawn", 2)

        for text, settle, label, nshots in STEPS:
            # The window closing mid-sequence invalidates every step after it,
            # and it did exactly that once. Say so at the moment it happens
            # rather than leaving it to be inferred from the pictures.
            probe = f"{OUT}/_probe.ppm"
            d("screenshot", probe, t=60)
            try:
                luma = ppm_mean_luma(probe)
                state = "window" if luma < 70 else "NO WINDOW"
                log(f"[step {label}] (pre-check luma={luma:.1f} -> {state})")
            except Exception as e:
                log(f"[step {label}] (pre-check failed: {e})")
            type_line(text)
            time.sleep(settle)
            shot(label, nshots)

    th.join(timeout=DRAIN + 100)

    try:
        with open(SERIAL_LOG, "r", errors="replace") as f:
            data = f.read()
        clean_txt = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "",
                           re.sub(r"\x1b[=>78]", "", data))
        open(f"{OUT}/serial.txt", "w").write(clean_txt[-1500000:])
        log(f"[serial -> {OUT}/serial.txt ({len(clean_txt)} B)]")
        for key in ("M20:", "ptytest", "PASS", "FAIL", "panic", "assertion",
                    "TIOCSCTTY", "TIOCSWINSZ", "brush-0.5", "uid="):
            n = clean_txt.count(key)
            if n:
                log(f"  serial: '{key}' x{n}")
    except Exception as e:
        log(f"[serial err] {e}")

    clean()
    log(f"==== M21 DONE -> {OUT} ====")


if __name__ == "__main__":
    main()
