#!/usr/bin/env python3
# M7k robust capture: ONLY short guest commands (staged scripts) to dodge the
# aarch64-HVF <40-char serial corruption. Launches a staged script, waits, dumps
# the busd ring. WHICH=comp -> /bin/m7kL.sh (real cosmic-comp); WHICH=coal -> /bin/m7kC.sh.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts/notes")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
WHICH= sys.argv[3] if len(sys.argv)>3 else "comp"
CWAIT= int(sys.argv[4]) if len(sys.argv)>4 else 70
TAG  = sys.argv[5] if len(sys.argv)>5 else WHICH
SCRIPT = "/bin/m7kL.sh" if WHICH=="comp" else "/bin/m7kC.sh"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    log(f"==== M7k run {ARCH} {MODE} {WHICH} tag={TAG} {time.ctime()} ====")
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,MODE,t=200)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("no boot"); clean(); return
    d("login","root","root",t=45)
    # single short command launches the staged script
    d("cmd", f"sh {SCRIPT} &", "12")
    log(f"[host sleep {CWAIT}s]"); time.sleep(CWAIT)
    # short dump command
    d("cmd","/bin/m7repro dump", t=60)
    d("cmd","echo POSTDUMP", t=8)
    try:
        with open(SERIAL_LOG,"r",errors="replace") as f: data=f.read()
        clean_txt=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        # ring window: from last DUMP begin to DUMP end
        i0=clean_txt.rfind("DUMP begin"); i1=clean_txt.rfind("DUMP end")
        ring=clean_txt[i0-6:i1+12] if (i0>=0 and i1>i0) else "(no ring)"
        dst=f"{OUT}/m7k-{WHICH}-{ARCH}-{TAG}.log"; open(dst,"w").write(ring)
        nR=ring.count("R7| t="); log(f"[ring->{dst} {nR} recs]")
        # busd.log slice from serial (busd redirected to file; grab last Listening..DUMP from log via a short cat is risky, so save full serial too)
        open(f"{OUT}/m7k-{WHICH}-{ARCH}-{TAG}-serial.txt","w").write(clean_txt[-300000:])
    except Exception as e: log(f"[err]{e}")
    clean(); log("==== run DONE ====")
if __name__=="__main__": main()
