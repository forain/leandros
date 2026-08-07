#!/usr/bin/env python3
# M5c: verify COSMIC_RENDER_DEVICE fix WITHOUT a baked launcher (launcher block
# was removed from committed mkfs). Set env inline in the persistent brush shell,
# then run /bin/cosmic-comp directly, drain serial via driver.py's own reader.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
RENDERDEV = sys.argv[3] if len(sys.argv) > 3 else "226:0"   # "none" to skip the fix
DUR = int(sys.argv[4]) if len(sys.argv) > 4 else 30
tag = "fix" if RENDERDEV != "none" else "ctrl"
DEST = f"{OUT}/m5c-inline-{tag}-{ARCH}-serial.log"
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
    log(f"==== M5c INLINE {ARCH} {MODE} render={RENDERDEV} dur={DUR} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-160:])
    env = "mkdir -p /run/user/0"
    d("cmd", env, "8", t=20)
    setline = "export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms SMITHAY_USE_LEGACY=1 ICED_BACKEND=tiny-skia COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 RUST_LOG=info"
    if RENDERDEV != "none":
        setline += f" COSMIC_RENDER_DEVICE={RENDERDEV}"
    d("cmd", setline, "8", t=20)
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6", t=15)
    log(f"--- cosmic-comp (render={RENDERDEV}, drain {DUR}s) ---")
    d("cmd", "cosmic-comp", str(DUR + 5), t=DUR + 40)
    d("screenshot", f"{OUT}/m5c-inline-{tag}-{ARCH}.ppm", t=30); log("[shot]")
    try: shutil.copy(SERIAL_LOG, DEST); log(f"[saved] {DEST} ({os.path.getsize(DEST)}B)")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5c INLINE DONE ====")
if __name__ == "__main__": main()
