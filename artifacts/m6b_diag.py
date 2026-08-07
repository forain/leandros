#!/usr/bin/env python3
# M6b BLOCKER-2 DIAGNOSIS v2: capture BOTH sides of the Wayland exchange when
# cosmic-bg dies at registry init. Builds pb.sh via SHORT echo-appends (the v1
# single long printf hit the guest tty MAX_CANON ~255B limit and truncated the
# compositor line -> comp never ran). Each line below is < ~110 chars.
#   comp:      RUST_LOG=info,smithay=debug  -> comp's disconnect reason
#   cosmic-bg: WAYLAND_DEBUG=1 RUST_BACKTRACE=1 -> last msg + any wl_display.error
# cosmic-bg ISOLATED (no panel). Fresh image assumed.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "d1"
LINES = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export RUST_LOG=info,smithay=debug",
    "echo NO-BUSD-MODE > /tmp/busd.log",
    "sleep 3",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
    "sleep 28",
    "export WAYLAND_DISPLAY=wayland-1",
    "export XDG_CONFIG_HOME=/root/.config XDG_DATA_DIRS=/usr/share",
    "export WAYLAND_DEBUG=1 RUST_BACKTRACE=1",
    "cosmic-bg >/tmp/bg.log 2>&1 &",
    "sleep 25",
]
# d4: NO busd (isolate busd as the comp-freeze cause vs M5f). LINES[2] left as the echo above.
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
    log(f"==== M6b DIAG v2 {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    # Build pb.sh line-by-line with short appends (each < MAX_CANON).
    d("cmd", "rm -f /tmp/pb.sh; echo START", "8")
    for ln in LINES:
        d("cmd", f"echo '{ln}' >> /tmp/pb.sh", "8")
    d("cmd", "wc -l /tmp/pb.sh", "8"); shot("script-wc")
    d("cmd", "/bin/sh /tmp/pb.sh >/tmp/pb.log 2>&1 & echo DIAG-LAUNCHED", "12")
    time.sleep(60)
    d("cmd", "echo ==WC==; wc -l /tmp/comp.log /tmp/bg.log /tmp/busd.log", "10"); shot("wc")
    d("cmd", "echo ==BG==; cat /tmp/bg.log", "10"); shot("bg-head")
    d("cmd", "echo ==BGTAIL==; tail -30 /tmp/bg.log", "10"); shot("bg-tail")
    d("cmd", "echo ==COMPTAIL==; tail -45 /tmp/comp.log", "12"); shot("comp-tail")
    d("cmd", "echo ==COMPT2==; tail -90 /tmp/comp.log | head -45", "12"); shot("comp-t2")
    d("cmd", "echo ==BUSD==; tail -12 /tmp/busd.log", "10"); shot("busd")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6b-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6b DIAG v2 DONE ====")
if __name__ == "__main__": main()
