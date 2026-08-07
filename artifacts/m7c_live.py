#!/usr/bin/env python3
# M7c: busd + comp both backgrounded with NO redirect -> output inherits the
# shell's serial tty (line-buffered, captured) while the shell stays usable.
# Defeats the file-redirect block-buffering that made both logs look empty.
# Prefix each process's lines so we can tell them apart in the interleaved serial.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
TAG  = sys.argv[3] if len(sys.argv)>3 else "live"
CWAIT= int(sys.argv[4]) if len(sys.argv)>4 else 75
RLOG = sys.argv[5] if len(sys.argv)>5 else "info"
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 RUST_BACKTRACE=full",
    "unset DISPLAY WAYLAND_DISPLAY",
]
# w.sh: no redirects -> both inherit the tty. busd first, then comp.
SCRIPT = [
    "rm -f /run/user/0/bus",
    "export RUST_LOG=busd=trace,zbus=trace,info",
    "/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus &",
    "sleep 4",
    "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    f"export RUST_LOG={RLOG}",
    "echo COMP_LAUNCH",
    "cosmic-comp --no-xwayland &",
    "echo COMP_BACKGROUNDED",
]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    log(f"==== M7c live {ARCH} {MODE} {TAG} rlog={RLOG} {time.ctime()} ====")
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,MODE,t=200)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("no boot"); clean(); return
    d("login","root","root",t=45)
    for e in ENV: d("cmd",e,"8")
    d("cmd","rm -f /tmp/w.sh; echo START","8")
    for ln in SCRIPT: d("cmd", f"echo '{ln}' >> /tmp/w.sh","8")
    d("cmd","/bin/sh /tmp/w.sh & echo LAUNCHED","12")
    log(f"[host sleep {CWAIT}s while comp runs, output streams to serial]"); time.sleep(CWAIT)
    d("cmd","echo M7C-LIVE-MARK",t=8)
    d("cmd","echo M7C-LIVE-MARK2",t=8)
    try:
        data=open(SERIAL_LOG,errors="replace").read()
        open(f"{OUT}/m7c-live-{ARCH}-{TAG}-serial.log","w").write(data[-400000:])
        log(f"[serial saved {len(data)}B]")
    except Exception as e: log(f"[serr]{e}")
    clean(); log("==== live DONE ====")
if __name__=="__main__": main()
