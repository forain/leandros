#!/usr/bin/env python3
# M6d W2 DIAG v3 — RELIABLE capture. comp stderr stays on the INHERITED SERIAL
# (clean program output, like kernel [FAULT] lines). The driver only appends to
# SERIAL_LOG while actively reading during a cmd, so we use `sleep N; echo MARK`
# driver cmds as READER-WINDOWS that pump comp's async stderr (W2DIAG + crash)
# into SERIAL_LOG. comp is idle (not CPU-starving) until a client connects, so
# the wlclient-launch cmd is sent cleanly; the composite+crash lands inside the
# following reader-window. No post-crash interactive cat (that path garbles).
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "e0"
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=info",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
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
    d("screenshot", f"{OUT}/m6d-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6d W2 DIAG3 {ARCH} {MODE} {TAG} {time.ctime()} ====")
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
    # Launch comp; stderr INHERITS the serial (no 2>&1). stdout -> file (quieter).
    d("cmd", "cosmic-comp --no-xwayland >/tmp/comp.log & echo COMPPID=$!", "10")
    # Reader-window 1: comp inits + takes fb (W2DIAG may fire on a self-composite).
    log("[reader window 1: comp init]")
    d("cmd", "sleep 34; echo COMPWINDOWEND", "48")
    shot("comp-up")
    # Launch wlclient (client wl_shm buffer -> composite -> the W2 crash path).
    d("cmd", "export WAYLAND_DISPLAY=wayland-1; wlclient >/tmp/wl.log 2>&1 & echo WLPID=$!", "12")
    # Reader-window 2: the composite + W2DIAG + softpipe crash stream to serial.
    log("[reader window 2: composite/crash]")
    d("cmd", "sleep 30; echo WLWINDOWEND", "44")
    shot("post")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6d-{ARCH}-{TAG}-serial.log")
    except Exception as ex: log(f"[serial err] {ex}")
    clean()
    log("==== M6d W2 DIAG3 DONE ====")
if __name__ == "__main__": main()
