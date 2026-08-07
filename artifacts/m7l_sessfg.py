#!/usr/bin/env python3
# M7l full session FOREGROUND: run start-cosmic-leandros with NO redirect so the
# whole chain (dbus-run-session -> busd + cosmic-session -> comp + panel +
# settings-daemon + notifications) streams line-buffered to the serial tty
# (a file redirect block-buffers and hides an early stall). Screenshot mid-run.
import subprocess, sys, os, time, shutil, re, threading
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7l-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"
CWAIT= int(sys.argv[3]) if len(sys.argv) > 3 else 75
TAG  = sys.argv[4] if len(sys.argv) > 4 else "fg0"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def clean(): d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M7l sessfg {ARCH} {MODE} cwait={CWAIT} tag={TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")): booted=True; break
    if not booted: log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    # Fire screenshots from a side thread while the launcher holds the tty.
    def shots():
        for delay, name in ((CWAIT-20,"a"),(CWAIT-6,"b")):
            time.sleep(delay if name=="a" else 14)
            d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-desk-{name}.ppm", t=30); log(f"[shot {name}]")
    th = threading.Thread(target=shots, daemon=True); th.start()
    log(f"[launcher FOREGROUND, capture {CWAIT}s]")
    out = d("cmd", "sh /bin/start-cosmic-leandros 2>&1", t=CWAIT)
    log("=== SESSION FOREGROUND STREAM ==="); log(deansi(out)[-14000:])
    th.join(timeout=5)
    d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-desk-c.ppm", t=30); log("[shot c]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m7l-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== sessfg DONE ====")
if __name__ == "__main__": main()
