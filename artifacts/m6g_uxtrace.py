#!/usr/bin/env python3
# M6g W1 UXTRACE — decisive: does comp emit a final SND (Hello) after the
# handshake with NO matching busd RCV (kernel delivery/wake bug), or no comp
# SND at all (comp-side async stall)? UXTRACE (kernel serial) prints
# "UXTR {SND|RCV|ACC|CON} pid= fd= v=" on every v>0 socket op. Shell-silent
# foreground script (busd+comp RUST_LOG->files, shell blocks in sleep => no
# prompt garble) so the async kernel UXTRACE lines land clean on serial.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
NBDRIVER = os.path.expanduser("~/code/leandros-artifacts/driver_nobreak.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "u0"
CSET = int(sys.argv[4]) if len(sys.argv) > 4 else 30
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
]
SCRIPT = [
    "rm -f /run/user/0/bus",
    "export RUST_LOG=busd=trace,info",
    "/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus 2>&1 &",
    "sleep 5",
    "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    "export RUST_LOG=warn",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
    f"sleep {CSET}",
    "echo UXEND",
]
def d(*a, t=200, drv=DRIVER):
    try:
        r = subprocess.run(["python3", drv, *a], capture_output=True, text=True, timeout=t)
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
    log(f"==== M6g UXTRACE {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    for e in ENV:
        d("cmd", e, "8")
    d("cmd", "rm -f /tmp/u.sh; echo START", "8")
    for ln in SCRIPT:
        d("cmd", f"echo '{ln}' >> /tmp/u.sh", "8")
    d("cmd", "wc -l /tmp/u.sh", "8")
    log("[silent foreground: UXTRACE serial lands clean]")
    out = d("cmd", "/bin/sh /tmp/u.sh", t=CSET + 40, drv=NBDRIVER)
    log("=== FOREGROUND WINDOW (tail) ==="); log(out[-3000:])
    shot("frozen")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6g-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6g UXTRACE DONE ====")
if __name__ == "__main__": main()
