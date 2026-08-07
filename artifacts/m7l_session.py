#!/usr/bin/env python3
# M7l FULL COSMIC SESSION via the full-path launcher start-cosmic-leandros
# (dbus-run-session -> busd + cosmic-session -> comp + panel + settings-daemon +
# notifications). Launch backgrounds; desktop screenshots are the milestone.
# TCG for a clean deterministic capture (HVF serial unreliable headless).
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7l-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"
WARM = int(sys.argv[3]) if len(sys.argv) > 3 else 85
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
    log(f"==== M7l SESSION {ARCH} {MODE} warm={WARM} tag={TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); clean(); return
    log("login " + d("login", "root", "root", t=45)[-60:])
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    # Launch (bg) + wait + dump evidence. All parsed at idle after the sleep.
    comp = (f"/bin/sh /bin/start-cosmic-leandros >/tmp/session.log 2>&1 & sleep {WARM}; "
            f"echo ===RUNDIR===; ls -la /run/user/0; echo ===PS===; ps | grep -E 'cosmic|busd' | grep -v grep; "
            f"echo ===MARK===; "
            f"echo comp=$(grep -ac 'cosmic_comp' /tmp/session.log) "
            f"panel=$(grep -ac 'cosmic-panel\\|cosmic_panel' /tmp/session.log) "
            f"settings=$(grep -ac 'settings.daemon\\|settings_daemon' /tmp/session.log) "
            f"notif=$(grep -ac 'notifications' /tmp/session.log) "
            f"wl=$(grep -ac 'WAYLAND_DISPLAY\\|Listening on wayland' /tmp/session.log) "
            f"panic=$(grep -ac 'panicked\\|stack overflow\\|recursion' /tmp/session.log) "
            f"readerr=$(grep -ac 'Socket reader task has errored' /tmp/session.log); "
            f"echo ===TAIL===; tail -40 /tmp/session.log; echo ===END===")
    total = WARM + 40
    log(f"--- compound (~{total}s) ---")
    proc = subprocess.Popen(["python3", DRIVER, "cmd", comp, str(total)],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    time.sleep(WARM - 10)
    d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-desktop.ppm", t=30); log("[shot desktop]")
    time.sleep(10)
    d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-desktop2.ppm", t=30); log("[shot desktop2]")
    try: out, _ = proc.communicate(timeout=total + 40)
    except subprocess.TimeoutExpired: proc.kill(); out = "(TIMEOUT)"
    log("=== COMPOUND OUTPUT ==="); log(out)
    d("screenshot", f"{OUT}/m7l-{ARCH}-{TAG}-desktop3.ppm", t=30); log("[shot desktop3]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m7l-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M7l SESSION DONE ====")
if __name__ == "__main__": main()
