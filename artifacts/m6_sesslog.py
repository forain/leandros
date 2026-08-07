#!/usr/bin/env python3
# Capture cosmic-session's ACTUAL stderr. Two isolated probes across the run:
#  E1: cosmic-comp --no-xwayland DIRECT (M5f repro) — still works? crash @0x1516B04?
#  E2: cosmic-session (via dbus) — its own stderr with RUST_BACKTRACE=full.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
WHICH = sys.argv[3] if len(sys.argv) > 3 else "E2"   # E1 or E2
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def shot(tag): d("screenshot", f"{OUT}/m6-sess-{ARCH}-{tag}.ppm", t=30); log(f"[shot {tag}]")
ENV = ("export XDG_RUNTIME_DIR=/run/user/0 HOME=/root COSMIC_BACKEND=kms "
       "COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 "
       "COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia "
       "PATH=/bin:/usr/bin RUST_LOG=debug RUST_BACKTRACE=full")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 SESSLOG {ARCH} {MODE} {WHICH} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", ENV, "8")
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6")
    if WHICH == "E1":
        d("cmd", "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 & echo COMP-BG", "10")
        time.sleep(20); shot("E1-comp-desktop")
        d("cmd", "head -40 /tmp/comp.log", "12"); time.sleep(1); shot("E1-comp-loghead")
        d("cmd", "tail -40 /tmp/comp.log", "12"); time.sleep(1); shot("E1-comp-logtail")
    else:
        d("cmd", "/bin/sh /usr/bin/dbus-run-session -- cosmic-session cosmic-comp --no-xwayland >/tmp/sess.log 2>&1 & echo SESS-BG", "10")
        time.sleep(18); shot("E2-desktop")
        d("cmd", "head -40 /tmp/sess.log", "12"); time.sleep(1); shot("E2-loghead")
        d("cmd", "tail -45 /tmp/sess.log", "12"); time.sleep(1); shot("E2-logtail")
        d("cmd", "wc -l /tmp/sess.log; ls -la /run/user/0", "10"); time.sleep(1); shot("E2-meta")
    clean()
    log("==== M6 SESSLOG DONE ====")
if __name__ == "__main__": main()
