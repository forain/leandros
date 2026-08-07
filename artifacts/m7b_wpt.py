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
d("cmd","wakepolltest >/tmp/wpt.log 2>&1; echo WPTRC=$?",t=90)
log(deansi(d("cmd","cat /tmp/wpt.log",t=25)))
clean(); log("WPT DONE")
