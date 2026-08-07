#!/usr/bin/env python3
# Regression check that the 8MB user-stack + mkfs additions didn't break the base
# system. Runs vfstest + drmsmoke via compound commands; screenshots the summaries.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 REGRESS {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    # vfstest: run to a file, show tail (summary). Compound so it survives.
    r = d("cmd", "vfstest > /tmp/vfs.txt 2>&1; echo VFSRC=$?; tail -6 /tmp/vfs.txt", "90")
    log("=== VFSTEST ==="); log(r[-1500:])
    time.sleep(1); d("screenshot", f"{OUT}/m6-regress-{ARCH}-vfstest.ppm", t=30); log("[shot vfstest]")
    r2 = d("cmd", "drmsmoke > /tmp/drm.txt 2>&1; echo DRMRC=$?; tail -8 /tmp/drm.txt", "60")
    log("=== DRMSMOKE ==="); log(r2[-1500:])
    time.sleep(1); d("screenshot", f"{OUT}/m6-regress-{ARCH}-drmsmoke.ppm", t=30); log("[shot drmsmoke]")
    r3 = d("cmd", "epolltest 2>&1 | tail -4; echo ---; scmtest 2>&1 | tail -4", "40")
    log("=== EPOLL/SCM ==="); log(r3[-1200:])
    time.sleep(1); d("screenshot", f"{OUT}/m6-regress-{ARCH}-misc.ppm", t=30); log("[shot misc]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6-regress-{ARCH}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6 REGRESS DONE ====")
if __name__ == "__main__": main()
