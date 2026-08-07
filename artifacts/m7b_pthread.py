import subprocess, os, time, re, sys
DRIVER=os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
def d(*a,t=200):
    try:
        r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s)
clean(); out=d("start","aarch64","uefi",t=200)
if not any(m in out for m in ("Login prompt ready","login:","Shell ready")): log("no boot"); clean(); sys.exit(1)
d("login","root","root",t=45)
for name,cmd,t in [("pthreadtest","pthreadtest",50),("vfstest","vfstest",95),("sigtest","sigtest",45),("timertest","timertest",45),("scmtest","scmtest",45),("epolltest","epolltest",45)]:
    log(f"=== {name} ===")
    d("cmd",f"{cmd} >/tmp/{name}.log 2>&1; echo {name}_RC=$?",t=t)
    log(deansi(d("cmd",f"tail -5 /tmp/{name}.log; echo RCWAS=$(grep -o '{name}_RC=[0-9]*' /dev/null 2>/dev/null)",t=15))[-800:])
    log(deansi(d("cmd",f"echo GREP:; grep -iE 'pass|fail|SUMMARY|ok|error' /tmp/{name}.log | tail -4",t=15))[-500:])
clean(); log("PT DONE")
