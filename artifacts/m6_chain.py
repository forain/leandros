#!/usr/bin/env python3
# M6 chain diagnostic: isolate WHY the launcher chain dies with empty output.
# Tests (a) brush-as-sh running a SCRIPT FILE (not -c), (b) dbus-run-session
# standalone, (c) path-resolution consistency on /run/user/0. Screenshot each.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
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
def shot(tag): d("screenshot", f"{OUT}/m6-chain-{ARCH}-{tag}.ppm", t=30); log(f"[shot {tag}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 CHAIN {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")

    # (A) brush-as-sh on a trivial SCRIPT FILE
    d("cmd", "printf 'echo HELLO_SCRIPT_A\\n' > /tmp/t.sh", "8")
    d("cmd", "/bin/sh /tmp/t.sh; echo rcA=$?", "10"); time.sleep(1); shot("A-scriptfile")

    # (B) path-resolution consistency on /run/user/0
    d("cmd", "test -d /run/user/0 && echo TESTD_OK || echo TESTD_NO", "8")
    d("cmd", "chmod 700 /run/user/0; echo chmodrc=$?", "8")
    d("cmd", "stat /run/user/0 2>&1 | head -3; echo statrc=$?", "8"); time.sleep(1); shot("B-pathres")

    # (C) dbus-run-session standalone with a trivial child
    d("cmd", "/bin/sh /usr/bin/dbus-run-session -- /bin/echo CHILD_RAN_C >/tmp/dc.log 2>&1; echo dbusrcC=$?", "25")
    d("cmd", "echo '--- dc.log ---'; cat /tmp/dc.log", "10"); time.sleep(1); shot("C-dbus")
    d("cmd", "ls -la /run/user/0", "8"); time.sleep(1); shot("C2-runuser")

    # (D) launcher with sh -x trace (backgrounded; it may exec into the session)
    d("cmd", "/bin/sh -x /bin/start-cosmic-leandros >/tmp/lt.log 2>&1 & echo TRACED", "10")
    time.sleep(10)
    d("cmd", "echo '--- lt.log tail ---'; tail -30 /tmp/lt.log", "12"); time.sleep(1); shot("D-trace")
    clean()
    log("==== M6 CHAIN DONE ====")
if __name__ == "__main__": main()
