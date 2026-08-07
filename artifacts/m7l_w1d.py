#!/usr/bin/env python3
# M7l: busd + cosmic-comp with BOTH logging to serial (NO file redirect) so
# busd's peer lifecycle (peer created / disconnected / errors) is captured in the
# same serial stream as comp's reader error + the kernel RXERR prints. Lets us
# see who tears down comp's session connection.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi-tcg"
TAG  = sys.argv[3] if len(sys.argv)>3 else "w1d"
CWAIT= int(sys.argv[4]) if len(sys.argv)>4 else 45
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
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    log(f"==== M7l w1d {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,MODE,t=200)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("no boot"); clean(); return
    d("login","root","root",t=45)
    for e in ENV: d("cmd",e,"8")
    d("cmd","rm -f /run/user/0/bus","8")
    # busd RUST_LOG=info -> peer lifecycle; NO redirect so it streams to the tty/serial.
    d("cmd","export RUST_LOG=info","8")
    d("cmd","/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus &","10")
    d("cmd","sleep 4; echo BUSD_UP","10")
    d("cmd","export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus","8")
    # comp FOREGROUND, unbuffered; both busd (bg) and comp now stream to serial.
    log(f"[run cosmic-comp FOREGROUND, capture {CWAIT}s]")
    out = d("cmd","cosmic-comp --no-xwayland 2>&1", t=CWAIT)
    log("=== COMP+BUSD FOREGROUND STREAM ===")
    log(deansi(out)[-12000:])
    try:
        data=open(SERIAL_LOG,errors="replace").read()
        open(f"{OUT}/m7l-w1d-{ARCH}-{TAG}-serial.log","w").write(data)
        log(f"[serial saved {len(data)}B]")
    except Exception as e: log(f"[serr]{e}")
    clean(); log("==== w1d DONE ====")
if __name__=="__main__": main()
