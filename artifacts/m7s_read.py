#!/usr/bin/env python3
# M7s: focused robust read of the panel's fate. Launch session -> settle ->
# read /tmp/panel.panic and /tmp/cs.log with short paths + retries (garble-robust).
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7s-logs")
ARCH="aarch64"; MODE="uefi-hvf"; SETTLE=int(sys.argv[1]) if len(sys.argv)>1 else 35
def d(*a,t=200):
    try:
        r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def rd(label, cmd, tries=5):
    log(f"=== {label} ===")
    for i in range(tries):
        out=deansi(d("cmd",cmd,"10"))
        # heuristic: a good read echoes the command intact
        if cmd.split()[-1] in out or "panic" in out.lower() or "code" in out.lower() or "exit" in out.lower():
            log(out[-3000:]); return
        time.sleep(1)
    log(f"[{label}] all {tries} reads garbled; last:"); log(out[-1500:])
def main():
    os.makedirs(OUT,exist_ok=True)
    log(f"==== M7s read {ARCH} {MODE} settle={SETTLE} {time.ctime()} ====")
    booted=False
    for a in range(1,4):
        log(f"#### BOOT {a} ####"); clean()
        o=d("start",ARCH,MODE,t=200)
        if any(m in o for m in ("Login prompt ready","login: ","Shell ready")): booted=True; break
    if not booted: log("FATAL no boot"); clean(); return
    d("login","root","root",t=45)
    d("cmd","export XDG_RUNTIME_DIR=/run/user/0","6")
    d("cmd","cd /tmp","5")
    d("cmd","rm -f panel.panic","5")
    log("[launch bg]")
    d("cmd","sh /bin/start-cosmic-leandros >/tmp/cs.log 2>&1 &","8")
    time.sleep(SETTLE)
    d("screenshot",f"{OUT}/m7s-read-t{SETTLE}.ppm",t=30)
    # Calm the console: stop the compositor so reads are reliable.
    d("cmd","pkill cosmic-comp","6"); time.sleep(2)
    d("cmd","pkill cosmic","6"); time.sleep(2)
    d("cmd","cd /tmp","5")
    rd("panel.panic","cat panel.panic",6)
    rd("cs.log(head)","head -c 3000 cs.log",6)
    rd("ps","ps",4)
    clean(); log("==== read DONE ====")
if __name__=="__main__": main()
