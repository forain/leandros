#!/usr/bin/env python3
# M7o inspection: bg launcher, settle, then robustly dump cs.log + ps to see the
# panel process state (no screenshots — avoids driver prompt desync).
import subprocess, sys, os, time, shutil, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7o-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"
CWAIT = int(sys.argv[3]) if len(sys.argv) > 3 else 60
def d(*a, t=220):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', s))
def clean():
    d("stop", t=30); subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M7o inspect {ARCH} {MODE} cwait={CWAIT} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    d("cmd", "sh /bin/start-cosmic-leandros >/tmp/cs.log 2>&1 &", "8")
    time.sleep(CWAIT)
    # settle prompt with a couple no-ops before the real queries
    d("cmd", "echo INSPECT_READY", "8")
    log("=== ps ==="); log(deansi(d("cmd", "ps", "12"))[-4000:])
    log("=== cosmic-panel present? ==="); log(deansi(d("cmd", "ps | grep -c panel", "10"))[-1000:])
    log("=== cs.log full ==="); log(deansi(d("cmd", "cat /tmp/cs.log", "15"))[-12000:])
    log("=== wl sockets ==="); log(deansi(d("cmd", "ls -la /run/user/0", "10"))[-2000:])
    try:
        shutil.copy(SERIAL_LOG, f"{OUT}/m7o-inspect-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== inspect DONE ====")
if __name__ == "__main__":
    main()
