#!/usr/bin/env python3
# Full COSMIC session bring-up. Launch backgrounds immediately, so the interactive
# shell is idle when the compound is typed -> the log dump (after `sleep`) runs
# without new serial input -> no garble. Desktop screenshot is the milestone.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
WARM = int(sys.argv[3]) if len(sys.argv) > 3 else 55
TAG = sys.argv[4] if len(sys.argv) > 4 else "s0"
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
    log(f"==== M6 SESSION {ARCH} {MODE} warm={WARM} tag={TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    log("login " + d("login", "root", "root", t=45)[-60:])
    # Preset XDG_RUNTIME_DIR so the launcher's `$(id -u)` path is skipped.
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    # Compound: launch (bg) + wait + dump evidence. All parsed at idle.
    comp = (f"/bin/sh /bin/start-cosmic-leandros >/tmp/session.log 2>&1 & sleep {WARM}; "
            f"echo ===RUNDIR===; ls -la /run/user/0; echo ===MARK===; "
            f"grep -ac 'variables from cosmic-comp' /tmp/session.log; "
            f"grep -ac 'Failed to request name' /tmp/session.log; "
            f"grep -ac panicked /tmp/session.log; "
            f"echo ===TAIL===; tail -35 /tmp/session.log; echo ===END===")
    total = WARM + 25
    log(f"--- compound (~{total}s) ---")
    proc = subprocess.Popen(["python3", DRIVER, "cmd", comp, str(total)],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    time.sleep(WARM - 8)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-desktop.ppm", t=30); log("[shot desktop]")
    time.sleep(8)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-desktop2.ppm", t=30); log("[shot desktop2]")
    try: out, _ = proc.communicate(timeout=total + 30)
    except subprocess.TimeoutExpired: proc.kill(); out = "(TIMEOUT)"
    log("=== COMPOUND OUTPUT ==="); log(out)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-console.ppm", t=30); log("[shot console]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6 SESSION DONE ====")
if __name__ == "__main__": main()
