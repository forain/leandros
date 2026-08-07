#!/usr/bin/env python3
# Short diagnostic: boot HVF, launch anvil, settle, dump serial (OPEN+PARK for fd identity).
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m4-screenshots")
ARCH, MODE = "aarch64", "uefi-hvf"
SETTLE = int(sys.argv[1]) if len(sys.argv) > 1 else 100
def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def boot():
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ####"); clean()
        os.environ["LEANDROS_QEMU_EXTRA"] = "-qmp unix:/tmp/leandros-qmp.sock,server,nowait"
        out = d("start", ARCH, MODE, t=170)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
    return False
def main():
    log(f"==== FDPROBE settle={SETTLE} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    d("cmd", "brush /bin/gorun &", t=8)
    time.sleep(SETTLE)
    import shutil
    try:
        shutil.copy("/tmp/leandros-serial.log", f"{OUT}/m4e-fdprobe-serial.log")
        log("serial copied")
    except Exception as e: log(f"copy err {e}")
    d("screenshot", f"{OUT}/m4e-fdprobe-A.ppm", t=30)
    log("==== FDPROBE DONE ====")
if __name__ == "__main__": main()
