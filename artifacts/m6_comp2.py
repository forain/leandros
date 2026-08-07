#!/usr/bin/env python3
# Robust comp-direct test via COMPOUND command typed at idle (M5f-proven; sleep
# exists). Everything after `&` is parsed before comp loads, so the log dump runs
# without new serial input -> no garble. Driver captures the compound's output.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
VARIANT = sys.argv[3] if len(sys.argv) > 3 else "home"   # home | tmpfs | min
CSET = int(sys.argv[4]) if len(sys.argv) > 4 else 24
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
    log(f"==== M6 COMP2 {ARCH} {MODE} {VARIANT} cset={CSET} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    log("login " + d("login", "root", "root", t=45)[-60:])
    if VARIANT == "min":
        env = ("export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 "
               "GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 "
               "COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia RUST_LOG=info")
    elif VARIANT == "tmpfs":
        d("cmd", "mkdir -p /tmp/h/.config /tmp/h/.cache /tmp/h/.local/share /tmp/rt; chmod 700 /tmp/rt", "10")
        env = ("export XDG_RUNTIME_DIR=/tmp/rt HOME=/tmp/h COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 "
               "GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 "
               "COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia XDG_CONFIG_HOME=/tmp/h/.config "
               "XDG_CACHE_HOME=/tmp/h/.cache XDG_DATA_HOME=/tmp/h/.local/share XDG_DATA_DIRS=/usr/share RUST_LOG=info")
    else:  # home
        env = ("export XDG_RUNTIME_DIR=/run/user/0 HOME=/root COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 "
               "GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 "
               "COSMIC_DISABLE_DIRECT_SCANOUT=1 ICED_BACKEND=tiny-skia RUST_LOG=info")
    d("cmd", env, "10")
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "6")
    compound = (f"cosmic-comp --no-xwayland >/tmp/cA.log 2>&1 & sleep {CSET}; "
                f"echo ===HEAD===; head -20 /tmp/cA.log; echo ===TAIL===; tail -18 /tmp/cA.log; "
                f"echo ===CFG===; ls -la /root/.config 2>&1; ls -la /tmp/h/.config 2>&1; echo ===END===")
    total = CSET + 20
    log(f"--- compound (~{total}s) ---")
    proc = subprocess.Popen(["python3", DRIVER, "cmd", compound, str(total)],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    time.sleep(CSET - 6)
    d("screenshot", f"{OUT}/m6-comp2-{ARCH}-{VARIANT}-desktop.ppm", t=30); log("[shot desktop]")
    try: out, _ = proc.communicate(timeout=total + 30)
    except subprocess.TimeoutExpired: proc.kill(); out = "(TIMEOUT)"
    log("=== COMPOUND OUTPUT ===")
    log(out)
    d("screenshot", f"{OUT}/m6-comp2-{ARCH}-{VARIANT}-console.ppm", t=30); log("[shot console]")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6-comp2-{ARCH}-{VARIANT}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6 COMP2 DONE ====")
if __name__ == "__main__": main()
