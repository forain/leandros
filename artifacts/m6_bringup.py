#!/usr/bin/env python3
# M6 bring-up: run the full COSMIC session via start-cosmic-leandros as a bg job,
# log to /tmp/session.log on tmpfs, screenshot concurrently, then extract bounded
# file-on-image evidence (HVF truncates long serial dumps, so keep each dump small).
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
WARM = int(sys.argv[3]) if len(sys.argv) > 3 else 60     # seconds to let the session paint
TAG  = sys.argv[4] if len(sys.argv) > 4 else "r0"

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {' '.join(str(x) for x in a)})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 BRINGUP {ARCH} {MODE} warm={WARM} tag={TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log("login: " + d("login", "root", "root", t=45)[-100:])

    # Preset XDG_RUNTIME_DIR so the launcher's `$(id -u)` substitution never runs,
    # and make the runtime dir. The launcher sets everything else itself.
    d("cmd", "mkdir -p /run/user/0 && chmod 700 /run/user/0 && echo RTDIR-OK", "10")
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    d("cmd", "export RUST_LOG=info,cosmic_comp::wayland=warn,cosmic_settings_config=off", "6")

    # Launch the whole session as a background job; all output to tmpfs.
    # Invoke via ABSOLUTE /bin/sh (kernel has no shebang binfmt; brush's exec
    # builtin does not fall through a nonexistent /usr/bin/sh).
    launch = ("/bin/sh /bin/start-cosmic-leandros >/tmp/session.log 2>&1 & echo M6-LAUNCHED")
    log("launch: " + d("cmd", launch, "12"))

    # Let it come up. Desktop screenshot partway and at the end.
    half = max(20, WARM // 2)
    time.sleep(half)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-a.ppm", t=30); log("[shot a desktop]")
    time.sleep(WARM - half)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-b.ppm", t=30); log("[shot b desktop]")

    # ---- console-screenshot evidence: the framebuffer console shows clean
    # ---- output (readline redraw noise is only in serial). Short cmds. ----
    d("cmd", "ls -la /run/user/0", "10"); time.sleep(1)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-runuser.ppm", t=30); log("[shot runuser]")
    d("cmd", "tail -22 /tmp/session.log", "12"); time.sleep(1)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-logtail.ppm", t=30); log("[shot logtail]")
    d("cmd", "head -22 /tmp/session.log", "12"); time.sleep(1)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-loghead.ppm", t=30); log("[shot loghead]")
    g = ("grep -aE 'variables from cosmic-comp|Failed to request name|panicked|"
         "Listening on|Starting cosmic-session|error' /tmp/session.log | tail -18")
    d("cmd", g, "12"); time.sleep(1)
    d("screenshot", f"{OUT}/m6-{ARCH}-{TAG}-grep.ppm", t=30); log("[shot grep]")

    try:
        shutil.copy(SERIAL_LOG, f"{OUT}/m6-{ARCH}-{TAG}-serial.log")
        log(f"[serial saved] {OUT}/m6-{ARCH}-{TAG}-serial.log")
    except Exception as e:
        log(f"[serial save err] {e}")
    clean()
    log("==== M6 BRINGUP DONE ====")

if __name__ == "__main__":
    main()
