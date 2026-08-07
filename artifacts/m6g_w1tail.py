#!/usr/bin/env python3
# M6g W1 TAIL CAPTURE — busd auth handshake COMPLETES (m6d_w1diag w1a proved it);
# comp then HANGS post-handshake (no crash, text console). w1a's cat dumps
# TRUNCATED before the decisive tail. This harness beats truncation: after the
# freeze, capture SMALL bounded tails/greps in SEPARATE driver cmds (FIFO drains
# between them). Goal: comp's last client-side zbus line (the method it blocks on,
# "waiting for reply") + busd's last server line (did it receive/route that
# MethodCall, e.g. Hello). The last request without a reply IS the freeze.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "t0"
CSET = int(sys.argv[4]) if len(sys.argv) > 4 else 40
LINES = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "rm -f /run/user/0/bus /tmp/busd.log /tmp/comp.log",
    "export RUST_LOG=busd=trace,zbus=trace,info RUST_BACKTRACE=1",
    "/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &",
    "sleep 4",
    "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
    "export RUST_LOG=zbus=trace,cosmic_comp=debug,warn",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
    f"sleep {CSET}",
]
# bounded capture cmds (each < ~15 lines out, separate window so FIFO drains)
CAPS = [
    "echo ==SZB==; wc -l /tmp/busd.log",
    "echo ==SZC==; wc -l /tmp/comp.log",
    "echo ==RUN==; ls -la /run/user/0",
    "echo ==BTAIL==; tail -30 /tmp/busd.log",
    "echo ==CTAIL==; tail -8 /tmp/comp.log",
    "echo ==PROC==; ps",
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
    d("screenshot", f"{OUT}/m6g-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    try: os.remove(SERIAL_LOG)
    except Exception: pass
    log(f"==== M6g W1 TAIL {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -f /tmp/w1.sh; echo START", "8")
    for ln in LINES:
        d("cmd", f"echo '{ln}' >> /tmp/w1.sh", "8")
    d("cmd", "wc -l /tmp/w1.sh", "8")
    d("cmd", "/bin/sh /tmp/w1.sh >/tmp/w1.log 2>&1 & echo W1-LAUNCHED", "12")
    time.sleep(CSET + 8); shot("frozen")
    for c in CAPS:
        out = d("cmd", c, "30"); time.sleep(1)
        log("=== CAP ==="); log(out[-1400:])
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6g-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6g W1 TAIL DONE ====")
if __name__ == "__main__": main()
