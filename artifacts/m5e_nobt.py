#!/usr/bin/env python3
# M5e: x86_64 llvmpipe with the CORRECTED loader env (GBM_ALWAYS_SOFTWARE=1 ->
# dri_screen_create_sw -> software=true, skips get_fd_render_gpu_drm). NO
# MESA_LOADER_DRIVER_OVERRIDE, NO LIBGL_ALWAYS_SOFTWARE (they mis-route to a
# non-software screen -> "compatible render device" fail). GALLIUM_DRIVER=llvmpipe
# picks llvmpipe inside kms_swrast. COSMIC_RENDER_DEVICE=226:0 bypasses is_software filter.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi" if ARCH == "x86_64" else "uefi-hvf")
DUR = int(sys.argv[3]) if len(sys.argv) > 3 else 30
TAG = sys.argv[4] if len(sys.argv) > 4 else "m5e-llvmpipe"
DEST = f"{OUT}/{TAG}-{ARCH}-serial.log"
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
    log(f"==== M5e LLVMPIPE {ARCH} {MODE} dur={DUR} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-160:])
    d("cmd", "mkdir -p /run/user/0", "8", t=20)
    setline = ("export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms SMITHAY_USE_LEGACY=1 "
               "ICED_BACKEND=tiny-skia COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 "
               "RUST_LOG=debug RUST_BACKTRACE=0 RUST_LIB_BACKTRACE=0 GBM_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe COSMIC_RENDER_DEVICE=226:0")
    d("cmd", setline, "8", t=20)
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6", t=15)
    log(f"--- cosmic-comp (llvmpipe, drain {DUR}s) ---")
    d("cmd", "cosmic-comp", str(DUR + 5), t=DUR + 40)
    d("screenshot", f"{OUT}/{TAG}-{ARCH}.ppm", t=30); log("[shot]")
    try: shutil.copy(SERIAL_LOG, DEST); log(f"[saved] {DEST} ({os.path.getsize(DEST)}B)")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5e LLVMPIPE DONE ====")
if __name__ == "__main__": main()
