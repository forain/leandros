#!/usr/bin/env python3
# M7r: capture cosmic-panel's swallowed panic. Instrumented panel writes the
# panic + backtrace to /tmp/panel.panic (survives launch_pad stderr discard).
# aarch64 HVF (stable session path). One boot attempt per discipline.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7r-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
WARM = int(sys.argv[3]) if len(sys.argv) > 3 else 55
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
    log(f"==== M7r PANEL PROBE {ARCH} {MODE} warm={WARM} {time.ctime()} ====")
    clean()
    out = d("start", ARCH, MODE, t=220)
    if not any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
        log("FATAL no boot:\n"+out[-500:]); clean(); return
    log("[boot ok] login " + d("login","root","root",t=45)[-40:])
    d("cmd","export XDG_RUNTIME_DIR=/run/user/0","6")
    d("cmd","rm -f /tmp/panel.panic","6")
    # Launch session with panel debug logging enabled; let it run + restart-loop.
    comp = ("export RUST_BACKTRACE=full; export RUST_LOG=cosmic_panel=info,warn; "
            f"/bin/sh /bin/start-cosmic-leandros >/tmp/session.log 2>&1 & sleep {WARM}; "
            "echo ===PS===; ps | head -45; "
            "echo ===PANELPANIC===; cat /tmp/panel.panic 2>/dev/null | head -20; "
            "echo ===SESSTAIL===; tail -70 /tmp/session.log; "
            "echo ===END===")
    total = WARM + 45
    proc = subprocess.Popen(["python3",DRIVER,"cmd",comp,str(total)],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    time.sleep(WARM - 8)
    d("screenshot", f"{OUT}/m7r-{ARCH}-desktop.ppm", t=30); log("[shot]")
    try: cout,_ = proc.communicate(timeout=total+40)
    except subprocess.TimeoutExpired: proc.kill(); cout="(TIMEOUT)"
    log("=== COMPOUND ==="); log(cout)
    clean()
    log("==== M7r PROBE DONE ====")
if __name__ == "__main__": main()
