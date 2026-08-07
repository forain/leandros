#!/usr/bin/env python3
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT=os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
def d(*a,t=200):
    try: r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    clean()
    ok=False
    for _ in range(2):
        o=d("start",ARCH,MODE,t=200)
        if any(m in o for m in ("Login prompt ready","login:","Shell ready")): ok=True; break
        clean()
    if not ok: log("no boot"); clean(); return
    d("login","root","root",t=45)
    o=deansi(d("cmd","vfstest",t=95))
    # reconstruct lines; find checks ending in PASS/FAIL, strip kernel spam chars
    open(f"{OUT}/m7c-vfsdiag-{ARCH}.raw","w").write(o)
    for ln in o.splitlines():
        s=ln.strip()
        if ": FAIL" in s or ": PASS" in s:
            # remove embedded kernel-spam fragments
            s2=re.sub(r'Task::new_kernel.*?allocation','',s)
            s2=re.sub(r'\[EXIT\][^\n]*','',s2)
            log("  "+s2[:180])
    log("---- lines mentioning FAIL ----")
    for ln in o.splitlines():
        if "FAIL" in ln: log("  >> "+ln.strip()[:200])
    clean()
if __name__=="__main__": main()
