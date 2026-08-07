#!/usr/bin/env python3
# M4e regression: fresh boot, vfstest FIRST, then the suite. arch + mode args.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-hvf"
def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def dcmd(c, t=90):
    o = d("cmd", c, t=t); log(f"\n$ {c}\n{o.strip()[-1400:]}"); return o
def boot():
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ({ARCH} {MODE}) ####"); clean()
        out = d("start", ARCH, MODE, t=170)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
    return False
def main():
    log(f"==== M4e REGRESSION {ARCH} {MODE} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    # vfstest FIRST on fresh image (discipline rule 5)
    dcmd("vfstest", t=120)
    for t in ["drmsmoke","scmtest","epolltest","evtest2","polltest","sigtest","timertest","waittest"]:
        dcmd(t, t=120)
    dcmd("idletest", t=60)
    log("==== REGRESSION DONE ====")
if __name__ == "__main__": main()
