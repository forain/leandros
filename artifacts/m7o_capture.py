#!/usr/bin/env python3
# M7o comp render-fault capture: boot aarch64 uefi-tcg (fresh image w/ AT-probe +
# EL0 backtrace + dump_user_vma(far) instrumentation ON), login root, run the full
# COSMIC session launcher FOREGROUND so the panel/applet render faults + [EXC]/
# [AT]/[BT]/[VMA] frames stream to the PL011 UART. Save the serial log for decode.
import subprocess, sys, os, time, shutil, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7o-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"
CWAIT = int(sys.argv[3]) if len(sys.argv) > 3 else 100
TAG = sys.argv[4] if len(sys.argv) > 4 else "cap0"
def d(*a, t=220):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', s))
def clean():
    d("stop", t=30); subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M7o cap {ARCH} {MODE} cwait={CWAIT} tag={TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted:
        log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    log(f"[launcher FOREGROUND, capture {CWAIT}s]")
    out = d("cmd", "sh /bin/start-cosmic-leandros", t=CWAIT)
    log("=== SESSION FOREGROUND STREAM (tail) ==="); log(deansi(out)[-8000:])
    d("screenshot", f"{OUT}/m7o-{ARCH}-{TAG}.ppm", t=30); log("[shot]")
    try:
        shutil.copy(SERIAL_LOG, f"{OUT}/m7o-{ARCH}-{TAG}-serial.log")
        log(f"[serial saved] {OUT}/m7o-{ARCH}-{TAG}-serial.log")
    except Exception as e:
        log(f"[serial err] {e}")
    clean(); log("==== cap DONE ====")
if __name__ == "__main__":
    main()
