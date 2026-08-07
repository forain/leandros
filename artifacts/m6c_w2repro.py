#!/usr/bin/env python3
# M6c W2 RECONFIRM: post-5c43227, does the softpipe FAR=0x10 pipe_get_tile_rgba(NULL)
# crash STILL fire when a CLIENT wl_shm buffer enters the composite path?
# No busd (comp reaches backend, per M6b Step14). wlclient (480x320, self-contained,
# smaller repro). Script-file via short echo-appends (avoids aarch64 HVF MAX_CANON trunc).
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "r0"
CSET = int(sys.argv[4]) if len(sys.argv) > 4 else 30
WSET = int(sys.argv[5]) if len(sys.argv) > 5 else 30
LINES = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=info",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
    f"sleep {CSET}",
    "echo RT; ls -la /run/user/0",
    "export WAYLAND_DISPLAY=wayland-1",
    "wlclient >/tmp/wl.log 2>&1 &",
    f"sleep {WSET}",
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
    d("screenshot", f"{OUT}/m6c-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6c W2 RECONFIRM {ARCH} {MODE} {TAG} {time.ctime()} ====")
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
    d("cmd", "/bin/sh /tmp/w.sh >/tmp/w.log 2>&1 & echo W2-LAUNCHED", "12")
    time.sleep(CSET + WSET // 2); shot("mid")
    time.sleep(WSET // 2 + 8); shot("end")
    # comp owns fb now; per-cmd serial dumps may garble but logs are on tmpfs.
    d("cmd", "pkill -9 wlclient; pkill -9 cosmic-comp; echo KILLED", "10"); time.sleep(3)
    d("cmd", "echo ==WL==; cat /tmp/wl.log; echo ==WLEND==", "12")
    d("cmd", "echo ==COMP==; tail -40 /tmp/comp.log; echo ==COMPEND==", "14")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6c-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6c W2 RECONFIRM DONE ====")
if __name__ == "__main__": main()
