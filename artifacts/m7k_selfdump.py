#!/usr/bin/env python3
# M7k HVF-safe capture: launch the SELF-DUMPING staged script, then send ZERO
# host->guest commands during the window (dodges HVF input corruption). The ring
# is dumped in-guest to serial; we read it from the serial log afterward.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"; OUT = os.path.expanduser("~/code/leandros-artifacts/notes")
ARCH = sys.argv[1] if len(sys.argv)>1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv)>2 else "uefi"
TAG  = sys.argv[3] if len(sys.argv)>3 else "sd"
WAIT = int(sys.argv[4]) if len(sys.argv)>4 else 60   # script self-dumps at 22s & 42s
GSCRIPT = sys.argv[5] if len(sys.argv)>5 else "/bin/m7kLD.sh"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def main():
    log(f"==== M7k selfdump {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,MODE,t=200)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("no boot"); clean(); return
    d("login","root","root",t=45)
    # ONE short command: launch the self-dumping script. No commands after this.
    d("cmd", f"sh {GSCRIPT} &", "12")
    # Keep a serial reader ACTIVE during the window (QEMU drops serial output with
    # no client) via a long-read noop; the in-guest self-dump streams into this read.
    log(f"[active-drain read {WAIT}s while guest self-dumps ring to serial]")
    d("cmd", f"sleep {WAIT-3}", t=WAIT+8)
    try:
        with open(SERIAL_LOG,"r",errors="replace") as f: data=f.read()
        clean_txt=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        i=max(clean_txt.rfind("COMP_LAUNCH"), clean_txt.rfind("CLIENT_LAUNCH"))
        window=clean_txt[i:] if i>=0 else clean_txt[-300000:]
        dst=f"{OUT}/m7k-sd-{ARCH}-{TAG}.log"; open(dst,"w").write(window)
        nd=window.count("DUMP begin"); nR=window.count("R7| t=")
        log(f"[serial->{dst} {len(window)}B, {nd} dumps, {nR} ring recs]")
    except Exception as e: log(f"[err]{e}")
    clean(); log("==== selfdump DONE ====")
if __name__=="__main__": main()
