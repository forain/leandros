#!/usr/bin/env python3
# M5e: kmscube (C, GLES2-over-gbm/kms_swrast) as the minimal llvmpipe canary.
# No Rust/backtrace to obscure a raw Mesa/LLVM failure. GALLIUM_DRIVER + GBM_ALWAYS_SOFTWARE.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi" if ARCH == "x86_64" else "uefi-hvf")
DUR = int(sys.argv[3]) if len(sys.argv) > 3 else 20
DRV = sys.argv[4] if len(sys.argv) > 4 else "llvmpipe"
TAG = f"m5e-kmscube-{DRV}"
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
    log(f"==== M5e KMSCUBE {DRV} {ARCH} {MODE} dur={DUR} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    d("cmd", f"export GALLIUM_DRIVER={DRV} GBM_ALWAYS_SOFTWARE=1 MESA_DEBUG=1 EGL_LOG_LEVEL=debug", "6", t=20)
    log(f"--- kmscube ({DRV}, {DUR}s) ---")
    d("cmd", f"kmscube -D /dev/dri/card0 -c {DUR*5}", str(DUR + 5), t=DUR + 40)
    d("screenshot", f"{OUT}/{TAG}-{ARCH}.ppm", t=30); log("[shot]")
    try: shutil.copy(SERIAL_LOG, DEST); log(f"[saved] {DEST} ({os.path.getsize(DEST)}B)")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5e KMSCUBE DONE ====")
if __name__ == "__main__": main()
