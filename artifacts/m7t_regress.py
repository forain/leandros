#!/usr/bin/env python3
# M7t regressions on the fresh (libwayland-egl) image: vfstest FIRST, then
# wakepolltest, then scmtest via scmrun.py. aarch64 HVF.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SCMRUN = os.path.expanduser("~/code/leandros/scripts/scmrun.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    log(f"==== M7t REGRESS {ARCH} {MODE} {time.ctime()} ====")
    clean()
    out = d("start", ARCH, MODE, t=220)
    if not any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
        log("FATAL no boot"); clean(); return
    log("[boot] "+d("login","root","root",t=45)[-30:])
    v = d("cmd","vfstest", t=120)
    log("=== VFSTEST tail ==="); log("\n".join(l for l in v.splitlines() if any(k in l for k in ("PASS","FAIL","Total","passed","failed","test","result")))[-1500:])
    w = d("cmd","wakepolltest", t=90)
    log("=== WAKEPOLLTEST tail ==="); log("\n".join(l for l in w.splitlines() if any(k in l for k in ("PASS","FAIL","Total","passed","failed","40")))[-1200:])
    sc = subprocess.run(["python3", SCMRUN, "scmtest", "60"], capture_output=True, text=True, timeout=100)
    so = (sc.stdout or "")
    log("=== SCMTEST tail ==="); log("\n".join(l for l in so.splitlines() if any(k in l for k in ("PASS","FAIL","passed","failed","/23","summary","SUMMARY")))[-1800:])
    clean(); log("==== M7t REGRESS DONE ====")
if __name__ == "__main__": main()
