#!/usr/bin/env python3
# PATH A: bring up the desktop WITHOUT cosmic-session (which spawns comp with
# COSMIC_SESSION_SOCK -> triggers comp's 0x1516B04 stack-overflow recursion).
# Run cosmic-comp directly under a bus, then cosmic-bg + cosmic-panel as manual
# Wayland clients. Yields panel + wallpaper on screen = the M6 milestone.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "a0"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
# The PATH-A script, built at idle before any heavy CPU load.
SCRIPT = (
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 "
    "GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 "
    "COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia RUST_LOG=warn\\n"
    "unset DISPLAY WAYLAND_DISPLAY\\n"
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &\\n"
    "sleep 14\\n"
    "export WAYLAND_DISPLAY=wayland-1\\n"
    "cosmic-bg >/tmp/bg.log 2>&1 &\\n"
    "cosmic-panel >/tmp/panel.log 2>&1 &\\n"
    "sleep 40\\n"
)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 PATHA {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -rf /root/.config /root/.cache /root/.local; echo CLEANED", "10")
    # Build the script at idle.
    d("cmd", f"printf '{SCRIPT}' > /tmp/patha.sh; wc -l /tmp/patha.sh", "10")
    # Run it under a private session bus (comp+bg+panel all share it). Backgrounded.
    d("cmd", "/bin/sh /usr/bin/dbus-run-session -- /bin/sh /tmp/patha.sh >/tmp/patha.log 2>&1 & echo PATHA-LAUNCHED", "12")
    time.sleep(42)
    d("screenshot", f"{OUT}/m6-patha-{ARCH}-{TAG}-desktop.ppm", t=30); log("[shot desktop]")
    time.sleep(10)
    d("screenshot", f"{OUT}/m6-patha-{ARCH}-{TAG}-desktop2.ppm", t=30); log("[shot desktop2]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6-patha-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6 PATHA DONE ====")
if __name__ == "__main__": main()
