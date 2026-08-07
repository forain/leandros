#!/usr/bin/env python3
# M7l comp-alone presentation check: busd (bg) + cosmic-comp (foreground), with a
# screendump mid-run. Bounds whether cosmic-comp presents to scanout with the W1
# fix, independent of the cosmic-session chain (which crashes at PID 5/6).
import subprocess, sys, os, time, shutil, re, threading
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7l-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"
CWAIT= int(sys.argv[3]) if len(sys.argv) > 3 else 55
TAG  = sys.argv[4] if len(sys.argv) > 4 else "cs0"
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 RUST_BACKTRACE=1",
    "unset DISPLAY WAYLAND_DISPLAY",
]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean(): d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M7l compshot {ARCH} {MODE} cwait={CWAIT} tag={TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")): booted=True; break
    if not booted: log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    for e in ENV: d("cmd", e, "6")
    d("cmd", "rm -f /run/user/0/bus", "6")
    d("cmd", "/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus &", "10")
    d("cmd", "sleep 4; echo BUSD_UP", "10")
    d("cmd", "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus", "6")
    def shots():
        time.sleep(CWAIT-18); d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-comp-a.ppm", t=30); log("[shot a]")
        time.sleep(12); d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-comp-b.ppm", t=30); log("[shot b]")
    th = threading.Thread(target=shots, daemon=True); th.start()
    log(f"[cosmic-comp FOREGROUND, capture {CWAIT}s]")
    out = d("cmd", "cosmic-comp --no-xwayland 2>&1", t=CWAIT)
    log("=== COMP STREAM ==="); log(deansi(out)[-9000:])
    th.join(timeout=5)
    d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-comp-c.ppm", t=30); log("[shot c]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m7l-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== compshot DONE ====")
if __name__ == "__main__": main()
