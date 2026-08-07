#!/usr/bin/env python3
# M7r verification: session up, panel no longer panics. Capture multiple timed
# screenshots + session.log tail + panel state (ps) to confirm the panel renders.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7r-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
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
    log(f"==== M7r VERIFY {ARCH} {MODE} {time.ctime()} ====")
    clean()
    out = d("start", ARCH, MODE, t=220)
    if not any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
        log("FATAL no boot"); clean(); return
    log("[boot] " + d("login","root","root",t=45)[-30:])
    d("cmd","export XDG_RUNTIME_DIR=/run/user/0","6")
    # launch session detached in guest; the compound just launches + returns.
    d("cmd","/bin/sh /bin/start-cosmic-leandros >/tmp/session.log 2>&1 &","8")
    # timed screenshots while the session settles/renders
    for wait,tag in ((45,"t45"),(65,"t65"),(85,"t85")):
        while True:
            # sleep in host between shots (driver.cmd sleeps would block serial)
            break
        time.sleep(20 if tag!="t45" else 45)
        d("screenshot", f"{OUT}/m7r-{ARCH}-{tag}.ppm", t=30); log(f"[shot {tag}]")
    # dump state (no grep in guest: use cat/tail/ps)
    st = d("cmd","echo ===PS===; ps | head -40; echo ===PANIC===; cat /tmp/panel.panic 2>/dev/null | head -8; echo ===SESSTAIL===; tail -50 /tmp/session.log; echo ===DONE===","30")
    log("=== STATE ==="); log(st[-4000:])
    clean(); log("==== M7r VERIFY DONE ====")
if __name__ == "__main__": main()
