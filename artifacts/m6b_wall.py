#!/usr/bin/env python3
# M6b WALLPAPER: no busd (comp reaches backend), comp + cosmic-bg, capture the COMPOSITED
# framebuffer via QMP screendump DURING the run (serial log-dumps garble once comp owns the fb).
# Compound-at-idle: build /tmp/w.sh via short echo appends, launch once, screenshot on timers.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "wall0"
LINES = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=warn",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
    "sleep 30",
    "export WAYLAND_DISPLAY=wayland-1 XDG_CONFIG_HOME=/root/.config XDG_DATA_DIRS=/usr/share",
    "cosmic-bg >/tmp/bg.log 2>&1 &",
    "sleep 90",
]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def shot(name):
    d("screenshot", f"{OUT}/m6b-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6b WALL {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -f /tmp/w.sh; echo START", "8")
    for ln in LINES:
        d("cmd", f"echo '{ln}' >> /tmp/w.sh", "8")
    d("cmd", "wc -l /tmp/w.sh", "8")
    # Launch once; DO NOT type over serial again until after screenshots (comp owns the fb).
    d("cmd", "/bin/sh /tmp/w.sh >/tmp/w.log 2>&1 & echo WALL-LAUNCHED", "12")
    # comp inits ~28s (fb goes black); cosmic-bg launches at ~30s and renders wallpaper.
    time.sleep(34); shot("t34-comp")      # comp up, fb black, bg not yet
    time.sleep(20); shot("t54-bg")        # cosmic-bg ~24s in -> wallpaper?
    time.sleep(20); shot("t74-bg")        # more settle
    time.sleep(20); shot("t94-bg")        # final wallpaper
    # Now dump logs (may garble, but screenshots are the deliverable). Kill comp first.
    d("cmd", "pkill -9 cosmic-comp; pkill -9 cosmic-bg; echo KILLED", "10"); time.sleep(3)
    d("cmd", "echo ==BG==; cat /tmp/bg.log; echo ==BGEND==", "12")
    d("cmd", "echo ==COMP==; tail -30 /tmp/comp.log; echo ==COMPEND==", "12")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6b-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6b WALL DONE ====")
if __name__ == "__main__": main()
