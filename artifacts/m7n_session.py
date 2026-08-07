#!/usr/bin/env python3
# M7n: launch the FULL COSMIC session after the execve signal-handler-reset fix.
# Boot, login root, run start-cosmic-leandros in the BACKGROUND (so the launcher's
# exec chain runs while we stay on a live shell), give it time to composite +
# spawn cosmic-session's children, then screenshot the GPU framebuffer.
# Success = NO [EXC]/[BT] fault, comp presents (dark bg + cursor), children spawn.
import subprocess, sys, os, time, shutil, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7n-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"
CWAIT = int(sys.argv[3]) if len(sys.argv) > 3 else 75
TAG = sys.argv[4] if len(sys.argv) > 4 else "s0"

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
    log(f"==== M7n session {ARCH} {MODE} cwait={CWAIT} tag={TAG} {time.ctime()} ====")
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
    # Launch in the BACKGROUND so the exec chain proceeds and we keep a shell.
    log(f"[launcher BACKGROUND, settle {CWAIT}s]")
    d("cmd", "sh /bin/start-cosmic-leandros >/tmp/cs.log 2>&1 &", "8")
    # Let the compositor come up and cosmic-session spawn its children.
    for k in range(CWAIT // 10):
        time.sleep(10)
        log(f"  ... settled {(k+1)*10}s")
    d("screenshot", f"{OUT}/m7n-{ARCH}-{TAG}.ppm", t=30); log("[shot]")
    # Pull the launcher log and process list for evidence.
    log("=== /tmp/cs.log (tail) ==="); log(deansi(d("cmd", "cat /tmp/cs.log", "10"))[-8000:])
    log("=== ps (session children) ==="); log(deansi(d("cmd", "ps", "8"))[-4000:])
    try:
        shutil.copy(SERIAL_LOG, f"{OUT}/m7n-{ARCH}-{TAG}-serial.log")
        log(f"[serial saved] {OUT}/m7n-{ARCH}-{TAG}-serial.log")
    except Exception as e:
        log(f"[serial err] {e}")
    clean(); log("==== session DONE ====")

if __name__ == "__main__":
    main()
