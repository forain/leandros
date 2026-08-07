#!/usr/bin/env python3
# Focused socket-server regression for the pending-accept fix: scmtest + epolltest + polltest
# (the suites that exercise handle_send/recv/poll/unix_stream_end). No drmsmoke (keeps the fb
# text console clean so summaries are readable). Each test -> file -> tail -> screenshot.
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
    log(f"==== M6b SOCKREG {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    for name in ("scmtest", "epolltest", "polltest"):
        r = d("cmd", f"{name} > /tmp/{name}.txt 2>&1; echo {name}RC=$?; tail -8 /tmp/{name}.txt", "70")
        log(f"=== {name} ==="); log(r[-1400:])
        time.sleep(1); d("screenshot", f"{OUT}/m6b-sockreg-{ARCH}-{name}.ppm", t=30); log(f"[shot {name}]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6b-sockreg-{ARCH}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6b SOCKREG DONE ====")
if __name__ == "__main__": main()
