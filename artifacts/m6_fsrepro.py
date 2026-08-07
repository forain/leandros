#!/usr/bin/env python3
# Narrow the f2fs directory bug + test tmpfs-XDG workaround for cosmic-comp config.
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
def shot(tag): d("screenshot", f"{OUT}/m6-fs-{ARCH}-{tag}.ppm", t=30); log(f"[shot {tag}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6 FSREPRO {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "clear", "5")
    # (A) f2fs nested-dir creation, one level at a time under a pre-created dir
    d("cmd", "mkdir /root/.config; echo mk1=$?", "8")
    d("cmd", "mkdir /root/.config/cosmic; echo mk2=$?", "8")
    d("cmd", "mkdir /root/.config/cosmic/x; echo mk3=$?", "8")
    d("cmd", "ls -la /root/.config; ls -la /root/.config/cosmic", "8")
    d("cmd", "chmod 755 /root/.config; echo chmodf2=$?", "8"); time.sleep(1); shot("A-f2fs-mkdir")
    # (B) same on tmpfs
    d("cmd", "clear", "5")
    d("cmd", "mkdir -p /tmp/xdg/cosmic/com.system76.CosmicComp/v1; echo mktmp=$?", "8")
    d("cmd", "ls -la /tmp/xdg/cosmic/com.system76.CosmicComp", "8")
    d("cmd", "chmod 700 /tmp/xdg; echo chmodtmp=$?", "8"); time.sleep(1); shot("B-tmpfs-mkdir")
    # (C) cosmic-comp with XDG dirs on tmpfs
    d("cmd", "clear", "5")
    env = ("export XDG_RUNTIME_DIR=/run/user/0 HOME=/tmp/home COSMIC_BACKEND=kms "
           "COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 "
           "COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia "
           "XDG_CONFIG_HOME=/tmp/home/.config XDG_CACHE_HOME=/tmp/home/.cache "
           "XDG_DATA_HOME=/tmp/home/.local/share XDG_DATA_DIRS=/usr/share XDG_CONFIG_DIRS=/etc/xdg "
           "PATH=/bin:/usr/bin RUST_LOG=info RUST_BACKTRACE=1")
    d("cmd", "mkdir -p /tmp/home/.config /tmp/home/.cache /tmp/home/.local/share; echo homerc=$?", "8")
    d("cmd", env, "8")
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6")
    d("cmd", "cosmic-comp --no-xwayland >/tmp/comp2.log 2>&1 & echo COMP2-BG", "10")
    time.sleep(20); shot("C-comp-tmpfsxdg-desktop")
    d("cmd", "tail -30 /tmp/comp2.log", "12"); time.sleep(1); shot("C-comp-tmpfsxdg-logtail")
    clean()
    log("==== M6 FSREPRO DONE ====")
if __name__ == "__main__": main()
