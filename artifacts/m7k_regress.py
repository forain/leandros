#!/usr/bin/env python3
# M7k regression run: vfstest FIRST, then the suite. Short guest commands.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi-tcg"
TESTS = ["vfstest","wakepolltest","epolltest","polltest","sigtest","timertest","idletest","pthreadtest","drmsmoke"]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    log(f"==== M7k regress {ARCH} {MODE} {time.ctime()} ====")
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,MODE,t=200)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("no boot"); clean(); return
    d("login","root","root",t=45)
    for t in TESTS:
        o=deansi(d("cmd", t, t=90))
        # last ~600 chars = summary
        log(f"----- {t} -----"); log(o[-700:])
    clean(); log("==== regress DONE ====")
if __name__=="__main__": main()
