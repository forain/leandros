#!/usr/bin/env python3
# M7c: launch comp BACKGROUNDED but with output to /dev/console (unbuffered,
# reaches the serial log) so the shell stays usable to probe busd + comp state.
# Goal: find WHERE comp hangs after config-load (backend/DRM? bus connect?).
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
TAG  = sys.argv[3] if len(sys.argv)>3 else "bg"
CWAIT= int(sys.argv[4]) if len(sys.argv)>4 else 75
RLOG = sys.argv[5] if len(sys.argv)>5 else "info"
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 RUST_BACKTRACE=full",
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
def dump(cmd,t=25,keep=6000): log(f"--- $ {cmd}"); log(deansi(d("cmd",cmd,t=t))[-keep:])
def main():
    log(f"==== M7c compbg {ARCH} {MODE} {TAG} rlog={RLOG} {time.ctime()} ====")
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,MODE,t=200)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("no boot"); clean(); return
    d("login","root","root",t=45)
    for e in ENV: d("cmd",e,"8")
    d("cmd","rm -f /run/user/0/bus","8")
    d("cmd","/usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &","10")
    d("cmd","sleep 4; echo BUSD_UP","10")
    d("cmd","export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus","8")
    d("cmd",f"export RUST_LOG={RLOG}","8")
    # comp backgrounded, output UNBUFFERED to console (serial-captured), pid saved
    d("cmd","cosmic-comp --no-xwayland >/dev/console 2>&1 & echo COMP_PID=$!","12")
    log(f"[host sleep {CWAIT}s while comp runs]"); time.sleep(CWAIT)
    log("=== is comp alive? ==="); dump("ps 2>/dev/null | head -40 || cat /proc/*/comm 2>/dev/null",t=20)
    log("=== busd.log (did comp connect / peer created?) ==="); dump("cat /tmp/busd.log",t=40,keep=9000)
    d("cmd","echo M7C-COMPBG-MARK",t=6)
    d("cmd","pkill -9 cosmic-comp; pkill -9 busd; echo KILLED","10")
    try:
        data=open(SERIAL_LOG,errors="replace").read()
        open(f"{OUT}/m7c-compbg-{ARCH}-{TAG}-serial.log","w").write(data[-260000:])
        log(f"[serial saved {len(data)}B]")
    except Exception as e: log(f"[serr]{e}")
    clean(); log("==== compbg DONE ====")
if __name__=="__main__": main()
