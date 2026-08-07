#!/usr/bin/env python3
# M5c: drive cosmic-comp through driver.py's OWN serial reader (single
# connection, no competing socket that lost the race in m5b_run.py).
# Boots, logs in, runs the launcher via `driver.py cmd` (which drains serial
# to SERIAL_LOG for `timeout` s), screenshots via the monitor socket, stops.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
LAUNCHER = sys.argv[3] if len(sys.argv) > 3 else "compfg"
DUR = int(sys.argv[4]) if len(sys.argv) > 4 else 25
DEST = f"{OUT}/m5c-{LAUNCHER}-{ARCH}-serial.log"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired as e:
        return f"(TIMEOUT {' '.join(a)}) " + (e.stdout or "" if hasattr(e,'stdout') else "")
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M5c {ARCH} {MODE} launcher={LAUNCHER} dur={DUR} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log("--- login ---"); log(d("login","root","root", t=45)[-200:])
    # run launcher; cmd drains serial into SERIAL_LOG for DUR seconds
    log(f"--- brush /bin/{LAUNCHER} (drain {DUR}s) ---")
    d("cmd", f"brush /bin/{LAUNCHER}", str(DUR + 5), t=DUR + 40)
    d("screenshot", f"{OUT}/m5c-{LAUNCHER}-{ARCH}.ppm", t=30); log("[shot]")
    # preserve the serial log for this run
    try: shutil.copy(SERIAL_LOG, DEST); log(f"[saved] {DEST} ({os.path.getsize(DEST)}B)")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5c DONE ====")
if __name__ == "__main__": main()
