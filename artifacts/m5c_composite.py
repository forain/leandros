#!/usr/bin/env python3
# M5c: cosmic-comp (GBM renderer via COSMIC_RENDER_DEVICE) + wl_shm client
# composite test. cosmic-comp runs in the background; wlclient connects on
# wayland-1; screenshot should show the client's colored buffer (non-black).
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
COMPWAIT = int(sys.argv[3]) if len(sys.argv) > 3 else 14
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
    log(f"==== M5c COMPOSITE {ARCH} {MODE} compwait={COMPWAIT} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    d("cmd", "mkdir -p /run/user/0", "6", t=15)
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 SMITHAY_USE_LEGACY=1 ICED_BACKEND=tiny-skia RUST_LOG=info,smithay::xwayland=error", "6", t=15)
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "5", t=15)
    log("--- launch cosmic-comp (bg) ---")
    # NOTE: redirect to /dev/null, NOT a /tmp file: tmpfs files cap at MAX_TMP_SIZE
    # (32 KiB); cosmic-comp's INFO logging overruns that and the write-past-cap
    # panics it. /dev/null keeps it alive; we judge success by the screenshot.
    d("cmd", "cosmic-comp >/dev/null 2>&1 &", str(COMPWAIT), t=COMPWAIT + 25)
    log("--- launch wlclient (bg) ---")
    d("cmd", "export WAYLAND_DISPLAY=wayland-1", "5", t=15)
    d("cmd", "wlclient >/tmp/wl.log 2>&1 &", "8", t=25)
    d("cmd", "true", "6", t=20)  # settle
    d("screenshot", f"{OUT}/m5c-composite-{ARCH}.ppm", t=30); log("[shot]")
    log("--- wl.log ---"); log(d("cmd", "cat /tmp/wl.log", "8", t=20)[-600:])
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m5c-composite-{ARCH}-serial.log")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5c COMPOSITE DONE ====")
if __name__ == "__main__": main()
