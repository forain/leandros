#!/usr/bin/env python3
# M7o: background launcher WITHOUT redirect so cosmic-session's tracing (stderr,
# line-flushed to the console PL011) streams to serial unbuffered. Grep for each
# "starting process" (one per session component spawn) to see if the panel is
# spawned or if cosmic-session hangs on the comp readiness handshake.
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
def clean():
    d("stop", t=30); subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M7o noredir {ARCH} {MODE} cwait={CWAIT} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    d("cmd", "sh /bin/start-cosmic-leandros &", "8")
    time.sleep(CWAIT)
    d("screenshot", f"{OUT}/m7o-noredir.ppm", t=30)
    try:
        shutil.copy(SERIAL_LOG, f"{OUT}/m7o-noredir-serial.log")
        raw = open(SERIAL_LOG, errors='replace').read()
        clean_s = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',raw))
        log("=== starting process lines ===")
        for ln in clean_s.splitlines():
            if 'starting process' in ln or 'cosmic-panel' in ln or 'cosmic-bg' in ln or 'cosmic_session' in ln or 'panic' in ln.lower() or 'error' in ln.lower():
                log(ln[:200])
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== noredir DONE ====")
if __name__ == "__main__":
    main()
