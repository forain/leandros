#!/usr/bin/env python3
# M4 EXIT driver (robust: driver.py cmd blessed path). All 3 exit criteria in one run:
#   1. wl_shm client roundtrip + composited  (wl.log "roundtrip done"+"configured -> painted", shot B)
#   2. cursor motion via QMP virtio-tablet    (shots C,D show cursor delta)
#   3. keyboard event reaches client          (wl.log "KEY code=", QMP key)
# Usage: m4d_exit.py <arch> <mode> <settle_s>   e.g. m4d_exit.py aarch64 uefi-hvf 90
import subprocess, sys, os, time

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP = os.path.expanduser("~/code/leandros-artifacts/m4-client/qmp.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m4-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-hvf"
SETTLE = int(sys.argv[3]) if len(sys.argv) > 3 else 90
TAG = f"{ARCH}-{MODE.replace('uefi-','').replace('uefi','tcg')}"

def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {' '.join(a)})"
def dcmd(c, t=12):
    o = d("cmd", c, t=t); log(f"$ {c}\n{o.strip()[-900:]}"); return o
def qmp(*a):
    try:
        r = subprocess.run(["python3", QMP, *a], capture_output=True, text=True, timeout=15)
        log(f"QMP {' '.join(a)} -> {(r.stdout or r.stderr).strip()[-200:]}")
    except Exception as e:
        log(f"QMP {' '.join(a)} ERR {e!r}")
def shot(name):
    p = f"{OUT}/m4d-{TAG}-{name}.ppm"
    d("screenshot", p, t=30); log(f"[shot] {p}")
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def boot():
    for attempt in range(1, 6):
        log(f"\n#### BOOT {attempt} ({ARCH} {MODE}) ####")
        clean()
        os.environ["LEANDROS_QEMU_EXTRA"] = "-qmp unix:/tmp/leandros-qmp.sock,server,nowait"
        out = d("start", ARCH, MODE, t=170)
        log(out[-400:])
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
    return False

def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M4 EXIT {ARCH} {MODE} settle={SETTLE} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-200:])
    log(f"\n---- anvil via /bin/gorun, settle {SETTLE}s ----")
    dcmd("brush /bin/gorun &", t=8)   # env+redirects live in the file (corruption-proof)
    time.sleep(SETTLE)
    dcmd("wc -l /tmp/anvil.log"); dcmd("tail -n 6 /tmp/anvil.log")
    shot("A-anvil")
    log("\n---- CRIT1: wl_shm client roundtrip + composite ----")
    dcmd("brush /bin/clrun &", t=8)
    time.sleep(22)
    dcmd("cat /tmp/wl.log")
    dcmd("tail -n 8 /tmp/anvil.log")
    shot("B-client")
    log("\n---- CRIT2: cursor via QMP virtio-tablet ----")
    qmp("move", "6000", "6000"); time.sleep(2); shot("C-cursor1")
    qmp("move", "26000", "20000"); time.sleep(2); shot("D-cursor2")
    log("\n---- CRIT3: keyboard event to client ----")
    qmp("key", "a"); time.sleep(1); qmp("key", "b"); time.sleep(2)
    dcmd("cat /tmp/wl.log")
    shot("E-key")
    import shutil
    try: shutil.copy("/tmp/leandros-serial.log", f"{OUT}/m4d-exit-{TAG}-serial.log")
    except Exception: pass
    log("==== M4 EXIT RUN DONE ====")

if __name__ == "__main__":
    main()
