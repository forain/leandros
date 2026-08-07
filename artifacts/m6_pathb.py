#!/usr/bin/env python3
# PATH B: busd (direct) + cosmic-comp WITHOUT a bus (so it doesn't stall pre-EGL
# like comp-under-bus does) + cosmic-bg/cosmic-panel WITH the bus as clients.
# Dump component logs after killing everything (idle shell) for clean evidence.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "b0"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
SCRIPT = (
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=warn\\n"
    "/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &\\n"
    "sleep 3\\n"
    "COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 "
    "COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &\\n"
    "sleep 16\\n"
    "export WAYLAND_DISPLAY=wayland-1 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus "
    "XDG_CONFIG_HOME=/root/.config XDG_DATA_DIRS=/usr/share\\n"
    "cosmic-bg >/tmp/bg.log 2>&1 &\\n"
    "cosmic-panel >/tmp/panel.log 2>&1 &\\n"
    "sleep 35\\n"
)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 PATHB {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -rf /root/.config /root/.cache /root/.local; echo CLEANED", "10")
    d("cmd", f"printf '{SCRIPT}' > /tmp/pb.sh; wc -l /tmp/pb.sh", "10")
    d("cmd", "/bin/sh /tmp/pb.sh >/tmp/pb.log 2>&1 & echo PB-LAUNCHED", "12")
    time.sleep(48)
    d("screenshot", f"{OUT}/m6-pathb-{ARCH}-{TAG}-desktop.ppm", t=30); log("[shot desktop]")
    time.sleep(6)
    d("screenshot", f"{OUT}/m6-pathb-{ARCH}-{TAG}-desktop2.ppm", t=30); log("[shot desktop2]")
    # kill the whole job, restore console, dump component logs cleanly.
    d("cmd", "kill %1 2>/dev/null; echo KILLED", "8"); time.sleep(4)
    d("cmd", "echo ==COMP==; tail -18 /tmp/comp.log", "12"); time.sleep(1)
    d("screenshot", f"{OUT}/m6-pathb-{ARCH}-{TAG}-complog.ppm", t=30); log("[shot complog]")
    d("cmd", "echo ==BG==; cat /tmp/bg.log; echo ==PANEL==; tail -12 /tmp/panel.log", "12"); time.sleep(1)
    d("screenshot", f"{OUT}/m6-pathb-{ARCH}-{TAG}-clientlog.ppm", t=30); log("[shot clientlog]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6-pathb-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6 PATHB DONE ====")
if __name__ == "__main__": main()
