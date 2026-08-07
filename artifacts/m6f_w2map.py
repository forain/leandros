#!/usr/bin/env python3
# M6f W2 MAP CAPTURE — clean Mesa-W2DIAG capture through the composite+crash.
# Prior e0-e3 garbled exactly at add_from_prime's completion because the shell's
# prompt redraw of the reader-window `echo MARK` cmd overwrote comp's async
# stderr. FIX (M6e shell-silent technique applied to Mesa stderr): run comp +
# wlclient from a FOREGROUND /bin/sh script whose comp/wlclient inherit the
# serial for stderr (NO 2>&1 redirect; stdout->/dev/null to cut noise). The
# interactive shell blocks inside /bin/sh (which blocks in sleep) => emits NO
# prompt => comp's W2DIAG add_from_prime / kms_map lines + kernel [FAULT] land
# clean. driver_nobreak reads the whole window (no early break on "> ").
# Goal: see, for the crashing frame, add_from_prime ret/handle/stride AND whether
# a W2DIAG kms_map OK/FAIL line follows (settles dt==NULL vs map-fail).
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
NBDRIVER = os.path.expanduser("~/code/leandros-artifacts/driver_nobreak.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "m0"
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=info",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
]
# script lines (built via short echo-appends; comp/wlclient stderr INHERIT serial)
SCRIPT = [
    "cosmic-comp --no-xwayland 1>/dev/null &",
    "sleep 8",
    "export WAYLAND_DISPLAY=wayland-1",
    "wlclient 1>/dev/null &",
    "sleep 60",
    "echo W2MAPEND",
]
def d(*a, t=200, drv=DRIVER):
    try:
        r = subprocess.run(["python3", drv, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def shot(name):
    d("screenshot", f"{OUT}/m6f-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    try: os.remove(SERIAL_LOG)
    except Exception: pass
    log(f"==== M6f W2 MAP {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    for e in ENV:
        d("cmd", e, "8")
    d("cmd", "rm -f /tmp/w.sh; echo START", "8")
    for ln in SCRIPT:
        d("cmd", f"echo '{ln}' >> /tmp/w.sh", "8")
    d("cmd", "wc -l /tmp/w.sh", "8")
    # FOREGROUND run: shell blocks ~68s in /bin/sh; no prompt => clean serial.
    log("[silent foreground window: comp+wlclient stderr -> serial]")
    d("cmd", "/bin/sh /tmp/w.sh", t=100, drv=NBDRIVER)
    shot("post")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6f-{ARCH}-{TAG}-serial.log")
    except Exception as ex: log(f"[serial err] {ex}")
    clean()
    log("==== M6f W2 MAP DONE ====")
if __name__ == "__main__": main()
