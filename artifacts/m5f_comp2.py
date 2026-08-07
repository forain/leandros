#!/usr/bin/env python3
# M5f: parametrized cosmic-comp fg present test. Args:
#   arch mode dur tag "cosmic-args" "precmd"
# Lets me test --no-xwayland, mkdir /tmp/.X11-unix, etc. + fd-count probe.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
DUR = int(sys.argv[3]) if len(sys.argv) > 3 else 40
TAG = sys.argv[4] if len(sys.argv) > 4 else "v2"
CARGS = sys.argv[5] if len(sys.argv) > 5 else ""      # e.g. "--no-xwayland"
PRECMD = sys.argv[6] if len(sys.argv) > 6 else ""     # e.g. "mkdir -p /tmp/.X11-unix"
DEST = f"{OUT}/m5f-{TAG}-{ARCH}-serial.log"
SHOT = f"{OUT}/m5f-{TAG}-{ARCH}.ppm"
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
    log(f"==== M5f {TAG} {ARCH} {MODE} dur={DUR} args='{CARGS}' pre='{PRECMD}' {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-160:])
    d("cmd", "mkdir -p /run/user/0", "8", t=20)
    if PRECMD:
        log("--- precmd ---"); log(d("cmd", PRECMD, "8", t=20)[-200:])
    setline = ("export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms "
               "COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 "
               "SMITHAY_USE_LEGACY=1 ICED_BACKEND=tiny-skia "
               "COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 RUST_LOG=info")
    d("cmd", setline, "8", t=20)
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6", t=15)
    cmd = f"cosmic-comp {CARGS}".strip()
    log(f"--- {cmd} (drain {DUR}s) ---")
    d("cmd", cmd, str(DUR + 5), t=DUR + 60)
    d("screenshot", SHOT, t=30); log(f"[shot] {SHOT}")
    try: shutil.copy(SERIAL_LOG, DEST); log(f"[saved] {DEST} ({os.path.getsize(DEST)}B)")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5f DONE ====")
if __name__ == "__main__": main()
