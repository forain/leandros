#!/usr/bin/env python3
# FRESH-image cosmic-comp DIRECT test (M5f repro). Robust against serial-input
# garbling: pre-stage the log-dump command as a tiny script while the shell is
# idle, then invoke it with ONE short command after comp settles. Screenshot.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
VARIANT = sys.argv[3] if len(sys.argv) > 3 else "min"   # min (M5f, no HOME) | home | tmpfs
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def shot(tag): d("screenshot", f"{OUT}/m6-fc-{ARCH}-{VARIANT}-{tag}.ppm", t=30); log(f"[shot {tag}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 FRESHCOMP {ARCH} {MODE} {VARIANT} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    # Pre-stage the dump script while idle (short cmd to run it later).
    d("cmd", "printf 'echo ===HEAD===; head -30 /tmp/cA.log; echo ===TAIL===; tail -20 /tmp/cA.log\\n' > /tmp/r.sh", "8")
    if VARIANT == "min":
        env = ("export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 "
               "GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 "
               "COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia RUST_LOG=info")
    elif VARIANT == "tmpfs":
        d("cmd", "mkdir -p /tmp/h/.config /tmp/h/.cache /tmp/h/.local/share /tmp/rt; chmod 700 /tmp/rt", "8")
        env = ("export XDG_RUNTIME_DIR=/tmp/rt HOME=/tmp/h COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 "
               "GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 "
               "COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia "
               "XDG_CONFIG_HOME=/tmp/h/.config XDG_CACHE_HOME=/tmp/h/.cache XDG_DATA_HOME=/tmp/h/.local/share "
               "XDG_DATA_DIRS=/usr/share RUST_LOG=info")
    else:  # home
        env = ("export XDG_RUNTIME_DIR=/run/user/0 HOME=/root COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 "
               "GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 "
               "COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia RUST_LOG=info")
    d("cmd", env, "8")
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6")
    d("cmd", "cosmic-comp --no-xwayland >/tmp/cA.log 2>&1 & echo CBG", "10")
    time.sleep(22); shot("desktop")
    d("cmd", "/bin/sh /tmp/r.sh", "15"); time.sleep(2); shot("log")
    clean()
    log("==== M6 FRESHCOMP DONE ====")
if __name__ == "__main__": main()
