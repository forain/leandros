#!/usr/bin/env python3
# M7s: capture the panel's 0x300xxxxx instruction-abort with EL0_BACKTRACE on.
# Boots aarch64 uefi-hvf, launches COSMIC, settles, dumps panel.panic + serial
# ([BT] frame.pt(ttbr0), [AT] par, [VMA]* backing/perms at ELR/FAR, [FAULT] IFSC).
import subprocess, sys, os, time, shutil, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7s-logs")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = "aarch64"
MODE = sys.argv[1] if len(sys.argv) > 1 else "uefi-hvf"
CWAIT = int(sys.argv[2]) if len(sys.argv) > 2 else 75
TAG = sys.argv[3] if len(sys.argv) > 3 else "fc2"
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
    log(f"==== M7s fc2 {ARCH} {MODE} cwait={CWAIT} {time.ctime()} ====")
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
    d("cmd", "rm -f /tmp/panel.panic", "5")
    log(f"[launcher BACKGROUND, settle {CWAIT}s]")
    d("cmd", "sh /bin/start-cosmic-leandros >/dev/console 2>&1 &", "8")
    step = 15
    for k in range(CWAIT // step):
        time.sleep(step)
        el = (k+1)*step
        d("screenshot", f"{OUT}/m7s-{ARCH}-{TAG}-t{el}.ppm", t=30)
        log(f"  ... {el}s [shot]")
    log("=== /tmp/panel.panic ===")
    log(deansi(d("cmd", "cat /tmp/panel.panic", "10")))
    log("=== /tmp/cs.log (tail) ===")
    log(deansi(d("cmd", "cat /tmp/cs.log", "12"))[-6000:])
    try:
        shutil.copy(SERIAL_LOG, f"{OUT}/m7s-{ARCH}-{TAG}-serial.log")
        log(f"[serial saved] {OUT}/m7s-{ARCH}-{TAG}-serial.log")
    except Exception as e:
        log(f"[serial err] {e}")
    clean(); log("==== fc2 DONE ====")
if __name__ == "__main__":
    main()
