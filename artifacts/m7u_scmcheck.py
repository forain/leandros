#!/usr/bin/env python3
import subprocess, os, time, re, sys
DRIVER=os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SCMRUN=os.path.expanduser("~/code/leandros-artifacts/scmrun.py")
OUT=os.path.expanduser("~/code/leandros-artifacts/notes")
ARCH=sys.argv[1] if len(sys.argv)>1 else "aarch64"
def d(*a,t=220):
    try: r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
clean()
out=d("start",ARCH,"uefi",t=220)
if not any(m in out for m in ("login:","Login prompt ready","Shell ready")):
    print("NOBOOT"); print(out[-1500:]); clean(); sys.exit(1)
d("login","root","root",t=45)
subprocess.run(["python3",SCMRUN,"echo WARMUP","4"],capture_output=True,text=True,timeout=20)
r=subprocess.run(["python3",SCMRUN,"scmtest","55"],capture_output=True,text=True,timeout=95)
txt=(r.stdout or "")+(r.stderr or "")
clean_txt=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',txt))
open(f"{OUT}/m7u-scmcheck-{ARCH}.txt","w").write(clean_txt)
np=len(re.findall(r'\bPASS\b',clean_txt)); nf=len(re.findall(r'\bFAIL\b',clean_txt))
print(f"scmtest PASS={np} FAIL={nf}")
for l in clean_txt.splitlines():
    if 'mincor' in l or 'FAIL' in l: print("  "+l.strip()[:120])
clean()
