#!/usr/bin/env python3
# M7b: run m7repro on LeandrOS with the kernel M7 syscall trace armed, capture
# the serial window. Shell-silent: we cat the repro's own stderr markers + the
# [M7> / [M7< kernel trace lines land on the same serial the driver logs.
import subprocess, sys, os, time, re, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi" if ARCH == "x86_64" else "uefi")
VARIANT = sys.argv[3] if len(sys.argv) > 3 else "A"
FLAVOR = sys.argv[4] if len(sys.argv) > 4 else "ct"
TAG = sys.argv[5] if len(sys.argv) > 5 else f"{VARIANT}{FLAVOR}"

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def main():
    log(f"==== M7b trace {ARCH} {MODE} variant={VARIANT} flavor={FLAVOR} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("FATAL no boot"); log(out[-2000:]); clean(); return
    d("login", "root", "root", t=45)
    # mark serial position, then run the repro foreground with a generous timeout
    marker = f"M7B-RUN-{TAG}"
    d("cmd", f"echo {marker}", t=8)
    log(f"[running /bin/m7repro {VARIANT} {FLAVOR}]")
    out = d("cmd", f"/bin/m7repro {VARIANT} {FLAVOR}", t=45)
    log("=== repro cmd output (markers) ===")
    log(out[-3000:])
    # pull the full serial window since our marker from the persistent log
    try:
        with open(SERIAL_LOG, "r", errors="replace") as f:
            data = f.read()
        idx = data.rfind(marker)
        window = data[idx:] if idx >= 0 else data[-20000:]
        dst = f"{OUT}/m7b-trace-{ARCH}-{TAG}.log"
        with open(dst, "w") as g: g.write(window)
        log(f"[serial window -> {dst}  ({len(window)} bytes)]")
        # echo the M7 trace lines + markers to stdout for immediate reading
        for ln in window.splitlines():
            if any(k in ln for k in ("[M7", "MARK", "POLLED", "WATCHDOG", "R:", "B:", "SUCCESS",
                                     "R7e ", "R7x ", "M7DUMP", "M:")):
                log(ln.rstrip())
    except Exception as e:
        log(f"[serial read err] {e}")
    clean(); log("==== DONE ====")

if __name__ == "__main__": main()
