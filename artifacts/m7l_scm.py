#!/usr/bin/env python3
# Boot + login, then run scmtest via scmrun.py's persistent serial reader
# (scmtest's "-> " diagnostics trip driver.py's early-break, so scmrun is required).
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SCMRUN = os.path.expanduser("~/code/leandros-artifacts/scmrun.py")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi-tcg"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    log(f"==== M7l scmtest {ARCH} {MODE} {time.ctime()} ====")
    booted=False
    for attempt in range(1,4):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,MODE,t=220)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("no boot"); clean(); return
    d("login","root","root",t=45)
    r = subprocess.run(["python3", SCMRUN, "scmtest", "50"], capture_output=True, text=True, timeout=90)
    out = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','', (r.stdout or "")+(r.stderr or ""))
    log("=== scmtest output ==="); log(out[-4000:])
    clean(); log("==== scmtest DONE ====")
if __name__=="__main__": main()
