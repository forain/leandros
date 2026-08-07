#!/usr/bin/env python3
# M7c f2fs crash-consistency validation for the flush-at-namespace-op fix.
# Test A (within-boot correctness): mkdir -p deep, stat in the same boot.
# Test B (the fix's target): mkdir + cache pressure + HARD-KILL qemu (no clean
#   unmount) + reboot the SAME image + stat -> intact WITH fix (was ?--------- baseline).
# Requires a FRESH image (caller rebuilds). Does NOT regenerate between kill+reboot.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s)
def hardkill(): subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(3)
def cleanstop(): d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def boot():
    for attempt in range(1,3):
        out=d("start",ARCH,MODE,t=200)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): return True
        hardkill()
    return False
def cmd(c,t=25): return deansi(d("cmd",c,t=t))
def main():
    log(f"==== M7c f2fs-crash {ARCH} {MODE} {time.ctime()} ====")
    # ---- BOOT 1: Test A + set up Test B, then HARD KILL ----
    hardkill()
    if not boot(): log("FATAL no boot 1"); hardkill(); return
    d("login","root","root",t=45)
    log("=== TEST A: within-boot mkdir -p correctness ===")
    log(cmd("mkdir -p /root/ta/tb/tc && echo AMK_OK; stat /root/ta/tb/tc; ls -la /root/ta/tb"))
    log("=== TEST B SETUP: mkdir + file, then cache pressure (NO sync) ===")
    log(cmd("mkdir /root/z1 && echo BMK_OK; echo hello_crash > /root/z1/file && echo BFILE_OK"))
    log(cmd("ls -la /root; stat /root/z1"))
    # cache pressure: touch many blocks so LRU evicts dir blocks out-of-band
    cmd("ls -la / /bin /usr /usr/share /usr/lib /etc /root >/dev/null 2>&1; echo PRESSURE_DONE", t=40)
    log("[HARD KILL qemu — NO clean unmount, NO sync]")
    hardkill()
    # ---- BOOT 2: same image, verify persistence ----
    log("=== REBOOT SAME IMAGE (no regen) ===")
    if not boot(): log("FATAL no boot 2 (mount may have failed after crash!)"); hardkill(); return
    d("login","root","root",t=45)
    log("=== TEST A RESULT (deep dir survived crash?) ===")
    log(cmd("stat /root/ta/tb/tc; ls -la /root/ta/tb"))
    log("=== TEST B RESULT (mkdir+file survived HARD KILL?) ===")
    log(cmd("stat /root/z1; ls -la /root; ls -la /root/z1; cat /root/z1/file"))
    log("[interpretation] WITH fix: /root/z1 is drwx, file readable 'hello_crash', ta/tb/tc intact.")
    log("[interpretation] Baseline (no fix): /root/z1 shows ?--------- (type 0) or ENOTDIR/absent.")
    cleanstop(); log("==== f2fs-crash DONE ====")
if __name__ == "__main__": main()
