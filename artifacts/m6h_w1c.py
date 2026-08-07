#!/usr/bin/env python3
# M6h W1 validation v3 — isolated busd(current_thread)+comp with the EXACT
# start-cosmic-leandros env + mkdir, so comp's config load succeeds and it
# actually connects to busd (the real W1 path). Diagnostics: ls -la of the
# config dirs (to explain any ENOTDIR), then head/tail/wc of comp.log & busd.log
# (grep is flaky/unshipped in this shell). If busd.log shows an accepted peer +
# Forwarding and comp.log proceeds past config into backend/wayland => W1 FIXED.
import subprocess, sys, os, time, shutil, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "v3"
CSET = int(sys.argv[4]) if len(sys.argv) > 4 else 45
ENV = [   # EXACT M6g proven env (comp ran + connected to busd there). Minimal.
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 RUST_BACKTRACE=1",
    "unset DISPLAY WAYLAND_DISPLAY",
]
SCRIPT = [
    "rm -f /run/user/0/bus",
    "export RUST_LOG=busd=trace,zbus=trace,info",
    "/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &",
    "sleep 5",
    "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    "export RUST_LOG=info",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def deansi(s):
    s=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s); s=re.sub(r'\x1b[=>78]','',s); return s
def dump(cmd, t=20, keep=1600):
    log(f"--- $ {cmd}"); log(deansi(d("cmd", cmd, t=t))[-keep:])
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6h W1c {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    # config-layout diagnostics BEFORE launch (fresh image => /root should be clean)
    log("=== CONFIG LAYOUT ===")
    dump("ls -la /root")
    dump("ls -la /bin/cosmic-comp 2>&1")   # confirm the binary is present+exec
    for e in ENV: d("cmd", e, "8")
    d("cmd", "rm -f /tmp/w.sh; echo START", "8")
    for ln in SCRIPT: d("cmd", f"echo '{ln}' >> /tmp/w.sh", "8")
    d("cmd", "/bin/sh /tmp/w.sh & echo LAUNCHED", "12")
    log(f"[host sleep {CSET}s]"); time.sleep(CSET)
    d("screenshot", f"{OUT}/m6h-{ARCH}-{TAG}-frozen.ppm", t=30); log("[shot]")
    log("=== RESULTS ===")
    dump("wc -l /tmp/busd.log /tmp/comp.log 2>&1", t=12)
    # busd.log FIRST (the W1 evidence), full cat, generous window
    dump("cat /tmp/busd.log", t=45, keep=9000)
    # comp progress: does it reach backend/render (=> Hello was replied)?
    dump("tail -30 /tmp/comp.log", t=30, keep=4000)
    d("cmd","pkill -9 cosmic-comp; pkill -9 busd; echo KILLED","10")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6h-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== M6h W1c DONE ====")
if __name__ == "__main__": main()
