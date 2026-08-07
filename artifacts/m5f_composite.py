#!/usr/bin/env python3
# M5f: cosmic-comp (bg) + wlclient wl_shm composite test. Screenshot should show
# wlclient's gradient (non-black). x86_64 softpipe committed stack + GBM_ALWAYS_SOFTWARE.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
COMPWAIT = int(sys.argv[3]) if len(sys.argv) > 3 else 16
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M5f COMPOSITE {ARCH} {MODE} compwait={COMPWAIT} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    d("cmd", "mkdir -p /run/user/0", "6", t=15)
    setline = ("export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms "
               "COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 "
               "SMITHAY_USE_LEGACY=1 ICED_BACKEND=tiny-skia "
               "COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 "
               "RUST_LOG=info,smithay::xwayland=error")
    d("cmd", setline, "6", t=15)
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "5", t=15)
    log("--- launch cosmic-comp --no-xwayland (bg, >/dev/null) ---")
    d("cmd", "cosmic-comp --no-xwayland >/dev/null 2>&1 &", str(COMPWAIT), t=COMPWAIT + 25)
    log("--- launch wlclient (bg) ---")
    d("cmd", "export WAYLAND_DISPLAY=wayland-1", "5", t=15)
    d("cmd", "wlclient >/tmp/wl.log 2>&1 &", "10", t=25)
    d("cmd", "true", "8", t=20)  # settle
    d("screenshot", f"{OUT}/m5f-composite-{ARCH}.ppm", t=30); log("[shot]")
    log("--- wl.log ---"); log(d("cmd", "cat /tmp/wl.log", "8", t=20)[-800:])
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m5f-composite-{ARCH}-serial.log")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5f COMPOSITE DONE ====")
if __name__ == "__main__": main()
