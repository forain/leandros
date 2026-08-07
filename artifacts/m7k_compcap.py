#!/usr/bin/env python3
# M7k: REAL cosmic-comp wedge capture. busd TRACED via armexec; comp launched
# m7c_live-style (full env, NO redirect -> inherits serial tty so we SEE it start
# / crash). Longer window. Dump busd ring at the wedge.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts/notes")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
TAG  = sys.argv[3] if len(sys.argv)>3 else "comp0"
CWAIT= int(sys.argv[4]) if len(sys.argv)>4 else 70
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 RUST_BACKTRACE=1",
    "unset DISPLAY WAYLAND_DISPLAY",
]
# busd traced (armexec), comp NO redirect (tty) so we see progress/crash.
SCRIPT = [
    "mkdir -p /run/user/0",
    "rm -f /run/user/0/bus",
    "export RUST_LOG=busd=trace,zbus=trace,info",
    "/bin/m7repro armexec /usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &",
    "sleep 5",
    "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    "export RUST_LOG=info",
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
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def dump(cmd,t=25,keep=8000): log(f"--- $ {cmd}"); log(deansi(d("cmd",cmd,t=t))[-keep:])
def main():
    log(f"==== M7k compcap {ARCH} {MODE} {TAG} {time.ctime()} ====")
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
    log(f"[host sleep {CWAIT}s for comp->busd wedge]"); time.sleep(CWAIT)
    log("=== busd.log (peer created? Waiting for message?) ==="); dump("cat /tmp/busd.log 2>&1", t=40, keep=9000)
    marker=f"M7K-COMP-{TAG}"; d("cmd",f"echo {marker}",t=6)
    log("[dump busd ring at the wedge]")
    d("cmd","/bin/m7repro dump",t=60)
    d("cmd","echo POSTDUMP",t=8)
    try:
        with open(SERIAL_LOG,"r",errors="replace") as f: data=f.read()
        idx=data.rfind(marker); window=data[idx:] if idx>=0 else data[-250000:]
        dst=f"{OUT}/m7k-comp-{ARCH}-{TAG}.log"; open(dst,"w").write(window)
        nR=window.count("R7| t="); log(f"[ring window->{dst} {len(window)}B {nR} recs]")
        # also save full busd.log region
        b0=data.rfind("Listening on UNIX");
        if b0>=0: open(f"{OUT}/m7k-comp-{ARCH}-{TAG}-busd.log","w").write(deansi(data[b0:idx if idx>b0 else b0+60000]))
    except Exception as e: log(f"[err]{e}")
    clean(); log("==== compcap DONE ====")
if __name__=="__main__": main()
