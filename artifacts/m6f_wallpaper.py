#!/usr/bin/env python3
# M6f TASK-3 visible composite: cosmic-comp + cosmic-bg (Orion wallpaper).
# W2 softpipe crash is FIXED (Step 27/28), so the wallpaper (blocked in M6b Step15
# by exactly that crash) should now paint. No busd (avoid the W1 comp-under-busd
# freeze). m6c-style: background the script, python-sleep, SCREENSHOT (serial
# garble is irrelevant now — we only need the framebuffer image).
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "bg0"
WARM = int(sys.argv[4]) if len(sys.argv) > 4 else 30
BGW  = int(sys.argv[5]) if len(sys.argv) > 5 else 30
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=info",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "export XDG_DATA_DIRS=/usr/share:/usr/local/share",
    "unset DISPLAY WAYLAND_DISPLAY",
]
SCRIPT = [
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
    f"sleep {WARM}",
    "export WAYLAND_DISPLAY=wayland-1",
    "cosmic-bg >/tmp/bg.log 2>&1 &",
    f"sleep {BGW}",
    "echo BGDONE",
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
    d("screenshot", f"{OUT}/m6f-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    try: os.remove(SERIAL_LOG)
    except Exception: pass
    log(f"==== M6f WALLPAPER {ARCH} {MODE} {TAG} {time.ctime()} ====")
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
    d("cmd", "/bin/sh /tmp/w.sh >/tmp/w.log 2>&1 & echo LAUNCHED", "12")
    time.sleep(WARM + BGW // 2); shot("mid")
    time.sleep(BGW // 2 + 6); shot("end")
    d("cmd", "pkill -9 cosmic-bg; pkill -9 cosmic-comp; echo KILLED", "12"); time.sleep(3)
    d("cmd", "echo ==BG==; tail -25 /tmp/bg.log; echo ==BGE==", "14")
    d("cmd", "echo ==COMP==; tail -15 /tmp/comp.log; echo ==COMPE==", "14")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6f-{ARCH}-{TAG}-serial.log")
    except Exception as ex: log(f"[serial err] {ex}")
    clean()
    log("==== M6f WALLPAPER DONE ====")
if __name__ == "__main__": main()
