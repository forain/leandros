#!/usr/bin/env python3
# M6 filesystem/sh ground-truth diagnostic. Runs short commands; the console
# screenshot shows clean final output (no readline-redraw noise).
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
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 DIAG {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    cmds = [
        "clear",
        "echo PATH=$PATH",
        "ls -la / | head -30",
        "mkdir -p /run/user/0; echo mkrc=$?",
        "ls -la /run 2>&1; ls -la /run/user 2>&1",
        "ls -la /bin/sh /usr/bin/dbus-run-session /usr/libexec/busd 2>&1",
        "/bin/sh -c 'echo SHOK_ABS'",
        "sh -c 'echo SHOK_BARE'",
        "command -v sh; command -v cosmic-comp",
    ]
    for c in cmds:
        log(f"$ {c}")
        d("cmd", c, "10")
        time.sleep(0.5)
    time.sleep(1)
    d("screenshot", f"{OUT}/m6-diag-{ARCH}.ppm", t=30); log("[shot]")
    # second batch after clear so it fits one screen
    d("cmd", "clear", "6")
    for c in cmds[3:]:
        d("cmd", c, "10"); time.sleep(0.4)
    time.sleep(1)
    d("screenshot", f"{OUT}/m6-diag-{ARCH}-b.ppm", t=30); log("[shot b]")
    clean()
    log("==== M6 DIAG DONE ====")
if __name__ == "__main__": main()
