#!/usr/bin/env python3
# M6e W2 TOKEN — clean deterministic capture of the kernel W2K FD2H / MAP_DUMB
# lines. Root cause of e3 garble: kernel serial_debug() and the shell's TTY
# output share the UART with no common lock, so the shell prompt redraw of the
# NEXT command truncated the async FD2H line mid-string. Fix: issue ONE command
# that backgrounds comp then blocks the shell in `sleep`, so the shell emits
# nothing while comp's FD2H/MAP_DUMB fire into a quiet UART. _serial_send reads
# the whole timeout into SERIAL_LOG; " -> handle=" / " -> NONE" contain "> " so
# the line is written to SERIAL_LOG and THEN it early-breaks — token captured.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "t0"
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=info",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
]
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def shot(name):
    d("screenshot", f"{OUT}/m6e-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    try: os.remove(SERIAL_LOG)
    except Exception: pass
    log(f"==== M6e W2 TOKEN {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    for e in ENV:
        d("cmd", e, "8")
    # ONE command: background comp, then BLOCK the shell in sleep. The shell
    # emits no prompt/echo during the sleep, so comp's async FD2H/MAP_DUMB
    # kernel serial_debug lines land clean. 55s window; comp reaches the
    # scanout dmabuf import (create_dumb -> HANDLE_TO_FD -> FD_TO_HANDLE) a few
    # seconds in. No client needed: FD2H fires on comp's OWN scanout import.
    log("[silent comp-init window: FD2H/MAP_DUMB fire here]")
    d("cmd", "cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 & sleep 55; echo WEND", "72")
    shot("post")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6e-{ARCH}-{TAG}-serial.log")
    except Exception as ex: log(f"[serial err] {ex}")
    clean()
    log("==== M6e W2 TOKEN DONE ====")
if __name__ == "__main__": main()
