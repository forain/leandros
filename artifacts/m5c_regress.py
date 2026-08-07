#!/usr/bin/env python3
# M5c: core regression — vfstest FIRST, then drmsmoke, on the fixed kernel.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
tests = sys.argv[3].split(",") if len(sys.argv) > 3 else ["vfstest","drmsmoke"]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    log(f"==== M5c REGRESS {ARCH} {MODE} {tests} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-100:])
    for tname in tests:
        log(f"===== RUN {tname} =====")
        out = d("cmd", f"/bin/{tname}", "90", t=140)
        out = out.replace("\r","")
        import re; out = re.sub(r'\x1b\[[0-9;?]*[a-zA-Z]', '', out)
        # print the tail (results summary)
        for l in out.splitlines():
            s=l.strip()
            if any(k in s for k in ("PASS","FAIL","pass","fail","OK","ok","tests","Tests","result","Result","/","summary","SUMMARY","total")) and "brush-0.5#" not in s:
                log("  "+s[:160])
        log(f"===== END {tname} =====")
    clean()
    log("==== M5c REGRESS DONE ====")
if __name__ == "__main__": main()
