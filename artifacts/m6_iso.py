#!/usr/bin/env python3
# Isolate the infinite-recursion crash: run cosmic-session with /bin/echo as a
# STUB compositor (under dbus). If cosmic-session logs "Starting cosmic-session"
# then panics on missing env -> cosmic-session early path OK, recursion is in COMP.
# If it crashes @0x1516B04 with empty log -> recursion is in cosmic-session itself.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
WHAT = sys.argv[3] if len(sys.argv) > 3 else "sess"   # sess (echo-comp) | comp (real comp under dbus)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 ISO {ARCH} {MODE} {WHAT} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -rf /root/.config /root/.cache /root/.local /tmp/cs.log; echo CLEANED", "10")
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia RUST_LOG=info RUST_BACKTRACE=1", "8")
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6")
    if WHAT == "sess":
        launch = "/bin/sh /usr/bin/dbus-run-session -- cosmic-session /bin/echo STUBCOMP >/tmp/cs.log 2>&1 & sleep 15; echo WOKE"
    else:  # real comp under dbus (session context: bus present, but comp run as the session's child directly)
        launch = "/bin/sh /usr/bin/dbus-run-session -- cosmic-comp --no-xwayland >/tmp/cs.log 2>&1 & sleep 18; echo WOKE"
    d("cmd", launch, "40")
    d("cmd", "wc -l /tmp/cs.log", "10"); time.sleep(1); d("screenshot", f"{OUT}/m6-iso-{ARCH}-{WHAT}-wc.ppm", t=30); log("[shot wc]")
    d("cmd", "head -34 /tmp/cs.log", "12"); time.sleep(1); d("screenshot", f"{OUT}/m6-iso-{ARCH}-{WHAT}-head.ppm", t=30); log("[shot head]")
    d("cmd", "tail -30 /tmp/cs.log", "12"); time.sleep(1); d("screenshot", f"{OUT}/m6-iso-{ARCH}-{WHAT}-tail.ppm", t=30); log("[shot tail]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6-iso-{ARCH}-{WHAT}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6 ISO DONE ====")
if __name__ == "__main__": main()
