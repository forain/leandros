#!/usr/bin/env python3
import subprocess, sys, os, time, re
DRIVER=os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH=sys.argv[1] if len(sys.argv)>1 else "aarch64"; MODE=sys.argv[2] if len(sys.argv)>2 else "uefi"
def d(*a,t=200):
    try:
        r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s)
clean(); out=d("start",ARCH,MODE,t=200)
if not any(m in out for m in ("Login prompt ready","login:","Shell ready")): log("no boot"); clean(); sys.exit(1)
d("login","root","root",t=45)
log("[running futextest]")
o=deansi(d("cmd","/bin/m7repro futextest",t=40))
for ln in o.splitlines():
    if any(k in ln for k in ("FUTEXTEST","T1 ","T2 ","T3 ","PASS","FAIL")): log(ln.rstrip())
clean(); log("FTX DONE")
