#!/usr/bin/env python3
# M6b DECISIVE: replicate M5f's WORKING setup (NO busd) on the new kernel (pending-accept
# fix), launch cosmic-bg instead of wlclient. Tests (1) does comp reach EGL/serving without
# a bus present, (2) does cosmic-bg roundtrip against a healthy comp with the fix.
# Hypothesis: busd presence makes comp block on a D-Bus call before EGL. No bus -> comp OK.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "n0"
# comp: M5f-identical env, NO busd, NO DBUS address. cosmic_settings_config=off, smithay=warn
# (so EGL/KMS/DRM backend WARN lines show = proof comp reached backend).
COMPLINES = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export RUST_LOG=info,cosmic_settings_config=off,smithay=warn",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
]
BGLINES = [
    "export WAYLAND_DISPLAY=wayland-1 HOME=/root",
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
    log(f"==== M6b NOBUS {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -f /tmp/pc.sh /tmp/pb.sh; echo START", "8")
    for ln in COMPLINES:
        d("cmd", f"echo '{ln}' >> /tmp/pc.sh", "8")
    for ln in BGLINES:
        d("cmd", f"echo '{ln}' >> /tmp/pb.sh", "8")
    d("cmd", "wc -l /tmp/pc.sh /tmp/pb.sh", "8")
    d("cmd", "/bin/sh /tmp/pc.sh >/tmp/pc.log 2>&1 & echo COMP-LAUNCHED", "12")
    time.sleep(32)
    # Did comp reach backend/serving? (EGL/KMS/DRM warn lines = proof.)
    d("cmd", "echo ==RUNDIR==; ls /run/user/0", "10"); shot("rundir")
    d("cmd", "echo ==COMP-PRE==; wc -l /tmp/comp.log; tail -22 /tmp/comp.log", "12"); shot("comp-pre")
    # Launch cosmic-bg against the (hopefully healthy) comp.
    d("cmd", "/bin/sh /tmp/pb.sh >/tmp/pb2.log 2>&1 & echo BG-LAUNCHED", "12")
    time.sleep(30)
    shot("desktop")
    d("cmd", "echo ==BG==; cat /tmp/bg.log", "10"); shot("bg")
    d("cmd", "echo ==COMP-POST==; wc -l /tmp/comp.log; tail -20 /tmp/comp.log", "12"); shot("comp-post")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6b-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6b NOBUS DONE ====")
if __name__ == "__main__": main()
