#!/usr/bin/env python3
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
TESTS = [("vfstest","vfstest",95),("f2fstest","f2fstest",60),("epolltest","epolltest",45),
         ("sigtest","sigtest",40),("timertest","timertest",40),("scmtest","scmtest",40),
         ("pthreadtest","pthreadtest",45)]
def d(*a,t=200):
    try: r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    log(f"==== M7c vcheck {ARCH} {MODE} {time.ctime()} ====")
    clean()
    ok=False
    for _ in range(2):
        o=d("start",ARCH,MODE,t=200)
        if any(m in o for m in ("Login prompt ready","login:","Shell ready")): ok=True; break
        clean()
    if not ok: log("no boot"); clean(); return
    d("login","root","root",t=45)
    for name,cmd,t in TESTS:
        o=deansi(d("cmd",cmd,t=t))
        npass=o.count(": PASS")+o.count(": ok")
        nfail=o.count(": FAIL")
        # also catch SUMMARY pass=/fail=
        m=re.search(r'pass[=:\s]+(\d+)\D+fail[=:\s]+(\d+)',o.lower())
        extra=f" [SUMMARY pass={m.group(1)} fail={m.group(2)}]" if m else ""
        verdict = "PASS" if (nfail==0 and (npass>0 or m)) else ("FAIL" if nfail>0 else "??")
        log(f"  {name}: {verdict}  (PASS-lines={npass} FAIL-lines={nfail}){extra}")
    clean(); log("==== vcheck DONE ====")
if __name__=="__main__": main()
