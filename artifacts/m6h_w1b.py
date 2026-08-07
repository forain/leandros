#!/usr/bin/env python3
# M6h W1 validation v2 — discrete captured probes (no foreground-token burial).
# busd(current_thread) + comp launched from a script file into log files; the
# HARNESS (host) sleeps; then each probe is a separate short cmd whose returned
# stdout is parsed individually. Decisive: busd FWD/WAIT (peer socket_reader
# polled + Hello routed) and comp.log tail (progress or the pid=14 panic reason).
import subprocess, sys, os, time, shutil, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "v1"
CSET = int(sys.argv[4]) if len(sys.argv) > 4 else 40
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "export XDG_DATA_DIRS=/usr/share:/usr/local/share",
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
PROBES = [
    "echo FWDMARK=$(grep -c Forwarding /tmp/busd.log)",
    "echo WAITMARK=$(grep -c Waiting /tmp/busd.log)",
    "echo BCASTMARK=$(grep -c Broadcasting /tmp/busd.log)",
    "echo BUSDLINES=$(wc -l < /tmp/busd.log)",
    "echo COMPLINES=$(wc -l < /tmp/comp.log)",
    "echo HELLOMARK=$(grep -c -i hello /tmp/busd.log)",
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
def grabmark(out, name):
    m=re.search(name+r'=([0-9]+)', deansi(out))
    return m.group(1) if m else "?"
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6h W1b {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    for e in ENV: d("cmd", e, "8")
    d("cmd", "rm -f /tmp/w.sh; echo START", "8")
    for ln in SCRIPT: d("cmd", f"echo '{ln}' >> /tmp/w.sh", "8")
    d("cmd", "/bin/sh /tmp/w.sh & echo LAUNCHED", "12")
    log(f"[host sleep {CSET}s for busd+comp to settle]"); time.sleep(CSET)
    d("screenshot", f"{OUT}/m6h-{ARCH}-{TAG}-frozen.ppm", t=30); log("[shot]")
    res={}
    for p in PROBES:
        name=p.split("=")[0].replace("echo ","")
        out=d("cmd", p, t=15); res[name]=grabmark(out, name)
    log("=== PROBE RESULTS ===")
    for k,v in res.items(): log(f"  {k} = {v}")
    log("=== ACCEPTED? ==="); log(deansi(d("cmd","echo ACC=$(grep -c Accepted /tmp/busd.log)", t=12))[-300:])
    log("=== COMP.LOG HEAD ==="); log(deansi(d("cmd","head -10 /tmp/comp.log", t=20))[-1600:])
    log("=== COMP.LOG TAIL ==="); log(deansi(d("cmd","tail -12 /tmp/comp.log", t=20))[-1400:])
    log("=== BUSD.LOG TAIL ==="); log(deansi(d("cmd","tail -14 /tmp/busd.log", t=20))[-1600:])
    d("cmd","pkill -9 cosmic-comp; pkill -9 busd; echo KILLED","10")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6h-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== M6h W1b DONE ====")
if __name__ == "__main__": main()
