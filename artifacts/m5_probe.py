#!/usr/bin/env python3
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def dcmd(c, t=40): o = d("cmd", c, t=t); log(f"\n$ {c}\n{o.strip()[-1800:]}"); return o
def boot():
    for a in range(1, 8):
        log(f"#### BOOT {a} ({ARCH} {MODE}) ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({a}) ####"); return True
    return False
def main():
    log(f"==== M5 PROBE {ARCH} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    dcmd("drmprobe", t=40)
    dcmd("ls -l /dev/dri/", t=20)
    clean(); log("==== PROBE DONE ====")
if __name__ == "__main__": main()
