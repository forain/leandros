import subprocess, sys, os, time, re
DRIVER=os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH="aarch64"; MODE="uefi"
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
log("[run futextest -> file]")
d("cmd","/bin/m7repro futextest >/tmp/ftx.log 2>&1; echo FTXRC=$?",t=45)
log(deansi(d("cmd","cat /tmp/ftx.log",t=20)))
clean(); log("DONE")
