#!/usr/bin/env python3
# M5f: robust cosmic-comp(--no-xwayland) + wlclient composite. Probes the
# wayland socket name, gives generous settle, captures wlclient stdout to
# /root/wl.log (short cwd path, no /tmp mangling), multiple screenshots.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
SETTLE = int(sys.argv[3]) if len(sys.argv) > 3 else 30
WLDISP = sys.argv[4] if len(sys.argv) > 4 else "wayland-1"
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
    log(f"==== M5f COMPOSITE2 {ARCH} {MODE} settle={SETTLE} disp={WLDISP} {time.ctime()} ====")
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
               "COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 RUST_LOG=info")
    d("cmd", setline, "6", t=15)
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "5", t=15)
    log(f"--- launch cosmic-comp --no-xwayland (bg), settle {SETTLE}s ---")
    d("cmd", "cosmic-comp --no-xwayland >/dev/null 2>&1 &", str(SETTLE), t=SETTLE + 25)
    log("--- probe runtime dir (wayland socket name) ---")
    log(d("cmd", "ls -la /run/user/0", "6", t=20)[-500:])
    log(f"--- launch wlclient (WAYLAND_DISPLAY={WLDISP}) ---")
    d("cmd", f"export WAYLAND_DISPLAY={WLDISP}", "4", t=12)
    d("cmd", "wlclient >/root/wl.log 2>&1 &", "10", t=25)
    d("cmd", "true", "8", t=20)  # settle for paint
    d("screenshot", f"{OUT}/m5f-composite2-{ARCH}.ppm", t=30); log("[shot1]")
    d("cmd", "true", "5", t=15)
    d("screenshot", f"{OUT}/m5f-composite2b-{ARCH}.ppm", t=30); log("[shot2]")
    log("--- wl.log ---"); log(d("cmd", "cat /root/wl.log", "6", t=18)[-900:])
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m5f-composite2-{ARCH}-serial.log")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5f COMPOSITE2 DONE ====")
if __name__ == "__main__": main()
