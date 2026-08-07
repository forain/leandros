#!/usr/bin/env python3
# M6g minimal robust vfstest capture: run to file, let it finish, cat the tail
# in a SHORT follow-up cmd (beats HVF read-window flakiness), verify via serial.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
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
    try: os.remove(SERIAL_LOG)
    except Exception: pass
    log(f"==== M6g VFS {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "vfstest >/tmp/v.txt 2>&1 & echo GO", "12")
    time.sleep(50)  # let vfstest finish
    r = d("cmd", "echo VR=$(grep -ac PASS /tmp/v.txt)/$(grep -ac FAIL /tmp/v.txt)", "20")
    log("=== PASS/FAIL COUNT ==="); log(r[-600:])
    r2 = d("cmd", "tail -4 /tmp/v.txt", "20")
    log("=== VFS TAIL ==="); log(r2[-800:])
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6g-vfs-{ARCH}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6g VFS DONE ====")
if __name__ == "__main__": main()
