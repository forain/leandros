#!/usr/bin/env python3
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    print(f"==== m7o vfs2 {ARCH} ====", flush=True)
    booted=False
    for a in range(1,3):
        clean(); o=d("start",ARCH,MODE,t=220)
        if any(m in o for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: print("FATAL"); clean(); return
    d("login","root","root",t=45)
    for _ in range(4): d("cmd","echo WU","8")
    o=deansi(d("cmd","vfstest","110"))
    npass=len(re.findall(r': PASS', o)); nfail=len(re.findall(r': FAIL', o))
    print(f"VFSTEST {ARCH}: PASS={npass} FAIL={nfail}")
    if nfail: 
        for ln in o.splitlines():
            if ': FAIL' in ln: print("  ", ln.strip()[:120])
    clean(); print("==== vfs2 DONE ====")
if __name__=="__main__": main()
