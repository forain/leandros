#!/usr/bin/env python3
# M7b: trace busd (armexec) against the REAL cosmic-comp client (coalesced Hello)
# = the exact W1 scenario. After comp connects and busd stalls, dump busd's ring.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
TAG  = sys.argv[3] if len(sys.argv)>3 else "c0"
CSET = int(sys.argv[4]) if len(sys.argv)>4 else 40
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 RUST_BACKTRACE=1",
    "unset DISPLAY WAYLAND_DISPLAY",
]
SCRIPT = [
    "mkdir -p /run/user/0",
    "rm -f /run/user/0/bus",
    "export RUST_LOG=busd=trace,zbus=trace,info",
    # busd launched TRACED via armexec (arms kernel ring for busd's tgid)
    "/bin/m7repro armexec /usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &",
    "sleep 5",
    "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    "export RUST_LOG=info",
    "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &",
]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def dump(cmd,t=25,keep=4000): log(f"--- $ {cmd}"); log(deansi(d("cmd",cmd,t=t))[-keep:])
def main():
    log(f"==== M7b comp-trace {ARCH} {MODE} {TAG} {time.ctime()} ====")
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
    log(f"[host sleep {CSET}s for comp->busd stall]"); time.sleep(CSET)
    log("=== busd.log (peer created? Forwarding?) ==="); dump("cat /tmp/busd.log 2>&1", t=40, keep=6000)
    log("=== comp.log tail ==="); dump("tail -20 /tmp/comp.log 2>&1", t=25, keep=3000)
    marker=f"M7B-COMP-{TAG}"; d("cmd",f"echo {marker}",t=6)
    log("[dump busd ring at the stall]")
    d("cmd","/bin/m7repro dump",t=45)
    d("cmd","echo POSTDUMP",t=8)
    d("cmd","pkill -9 cosmic-comp; pkill -9 busd; echo KILLED","10")
    try:
        with open(SERIAL_LOG,"r",errors="replace") as f: data=f.read()
        idx=data.rfind(marker); window=data[idx:] if idx>=0 else data[-80000:]
        dst=f"{OUT}/m7b-comp-{ARCH}-{TAG}.log"; open(dst,"w").write(window)
        nR=window.count("R7e ")+window.count("R7x "); log(f"[ring window->{dst} {len(window)}B {nR} recs]")
    except Exception as e: log(f"[err]{e}")
    clean(); log("==== comp-trace DONE ====")
if __name__=="__main__": main()
