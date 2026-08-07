#!/usr/bin/env python3
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH="aarch64"; MODE="uefi"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    clean(); out=d("start",ARCH,MODE,t=200)
    if not any(m in out for m in ("Login prompt ready","login:","Shell ready")):
        log("no boot"); clean(); return
    d("login","root","root",t=45)
    d("cmd","mkdir -p /run/user/0; export XDG_RUNTIME_DIR=/run/user/0; rm -f /run/user/0/bus; echo SET",t=10)
    # is busd present + runnable?
    log(d("cmd","ls -la /usr/libexec/busd 2>&1; ls -la /bin/m7repro /bin/w1client 2>&1",t=10))
    # start busd traced
    d("cmd","/bin/m7repro armexec /usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &",t=10)
    log("[busd started; sleeping 4]")
    log(d("cmd","sleep 4; echo BUSD_LOG:; cat /tmp/busd.log 2>&1 | head -40",t=20))
    log(d("cmd","ls -la /run/user/0/ 2>&1",t=8))
    marker="M7B-DIAG"
    d("cmd",f"echo {marker}",t=6)
    log("[dump ring — busd startup/park state, NO client yet]")
    d("cmd","/bin/m7repro dump",t=40)
    d("cmd","echo POSTDUMP",t=8)
    try:
        with open(SERIAL_LOG,"r",errors="replace") as f: data=f.read()
        idx=data.rfind(marker); window=data[idx:] if idx>=0 else data[-40000:]
        dst=f"{OUT}/m7b-busddiag-{ARCH}.log"; open(dst,"w").write(window)
        nR=window.count("R7e ")+window.count("R7x "); log(f"[window->{dst} {len(window)}B, {nR} ring recs]")
    except Exception as e: log(f"[err]{e}")
    clean(); log("DIAG DONE")
if __name__=="__main__": main()
