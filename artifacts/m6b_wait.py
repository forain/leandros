#!/usr/bin/env python3
# M6b comp-READINESS test (no rebuild): give comp 55s alone, inspect /run/user/0 for the
# wayland socket + full comp.log (does comp reach a serving/listening state?), THEN launch
# cosmic-bg. Separates "comp slow-to-accept" (race, fixable) from "comp stuck in init".
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "w0"
LINES = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export RUST_LOG=info",
    "/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &",
    "sleep 3",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
]
BGLINES = [
    "export WAYLAND_DISPLAY=wayland-1 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    "export XDG_CONFIG_HOME=/root/.config XDG_DATA_DIRS=/usr/share WAYLAND_DEBUG=1 RUST_BACKTRACE=1",
    "cosmic-bg >/tmp/bg.log 2>&1 &",
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
    d("screenshot", f"{OUT}/m6b-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]"); time.sleep(1)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6b WAIT {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -f /tmp/pc.sh /tmp/pb.sh; echo START", "8")
    for ln in LINES:
        d("cmd", f"echo '{ln}' >> /tmp/pc.sh", "8")
    for ln in BGLINES:
        d("cmd", f"echo '{ln}' >> /tmp/pb.sh", "8")
    d("cmd", "wc -l /tmp/pc.sh /tmp/pb.sh", "8")
    # launch comp-only script, then wait 55s for comp to fully init
    d("cmd", "/bin/sh /tmp/pc.sh >/tmp/pc.log 2>&1 & echo COMP-LAUNCHED", "12")
    time.sleep(55)
    # Is comp serving? Check the socket + full comp log BEFORE any client.
    d("cmd", "echo ==RUNDIR==; ls -la /run/user/0", "10"); shot("rundir")
    d("cmd", "echo ==COMP-PRE==; wc -l /tmp/comp.log; tail -20 /tmp/comp.log", "12"); shot("comp-pre")
    # Now launch cosmic-bg against the (hopefully ready) comp.
    d("cmd", "/bin/sh /tmp/pb.sh >/tmp/pb2.log 2>&1 & echo BG-LAUNCHED", "12")
    time.sleep(14)
    shot("desktop")
    d("cmd", "echo ==BG==; cat /tmp/bg.log", "10"); shot("bg")
    d("cmd", "echo ==COMP-POST==; wc -l /tmp/comp.log; tail -25 /tmp/comp.log", "12"); shot("comp-post")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6b-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6b WAIT DONE ====")
if __name__ == "__main__": main()
