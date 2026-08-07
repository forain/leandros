#!/usr/bin/env python3
# Capture cosmic-session's log AFTER the crash (idle shell -> clean, no garble).
# Cleans cosmic state first to avoid persistent-image pollution.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
WARM = int(sys.argv[3]) if len(sys.argv) > 3 else 25
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
    log(f"==== M6 SLOG {ARCH} {MODE} warm={WARM} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -rf /root/.config /root/.cache /root/.local /tmp/session.log; echo CLEANED", "10")
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    # Launch + wait for the (fast) crash. Compound so nothing typed during load.
    d("cmd", f"/bin/sh /bin/start-cosmic-leandros >/tmp/session.log 2>&1 & sleep {WARM}; echo WOKE",
      str(WARM + 15))
    # Session has crashed by now -> shell idle -> clean dumps.
    d("cmd", "grep -c 'variables from cosmic-comp' /tmp/session.log; grep -c overflow /tmp/session.log; grep -c panicked /tmp/session.log; wc -l /tmp/session.log", "12")
    time.sleep(1); d("screenshot", f"{OUT}/m6-slog-{ARCH}-counts.ppm", t=30); log("[shot counts]")
    d("cmd", "tail -42 /tmp/session.log", "15")
    time.sleep(1); d("screenshot", f"{OUT}/m6-slog-{ARCH}-tail.ppm", t=30); log("[shot tail]")
    d("cmd", "head -30 /tmp/session.log", "15")
    time.sleep(1); d("screenshot", f"{OUT}/m6-slog-{ARCH}-head.ppm", t=30); log("[shot head]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6-slog-{ARCH}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6 SLOG DONE ====")
if __name__ == "__main__": main()
