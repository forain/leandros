#!/usr/bin/env python3
# M5f criterion 3: busd session bus on-target + cosmic-comp (a zbus client)
# RequestName's com.system76.CosmicComp. Start busd, export the address, start
# cosmic-comp; capture busd.log + cosmic-comp serial for the name acquisition.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
WAIT = int(sys.argv[3]) if len(sys.argv) > 3 else 22
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M5f BUSD {ARCH} {MODE} wait={WAIT} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); booted = True; break
    if not booted:
        log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    d("cmd", "mkdir -p /run/user/0", "6", t=15)
    # One compound line (idle shell): busd -> address -> cosmic-comp -> report.
    compound = (
        "export XDG_RUNTIME_DIR=/run/user/0; rm -f /run/user/0/bus; "
        "/usr/libexec/busd --config /usr/share/dbus-1/session.conf "
        "--address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 & sleep 4; "
        "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus; "
        "echo BUSD-ADDR $DBUS_SESSION_BUS_ADDRESS; "
        "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 "
        "SMITHAY_USE_LEGACY=1 ICED_BACKEND=tiny-skia COSMIC_DISABLE_SYNCOBJ=1 "
        "COSMIC_DISABLE_DIRECT_SCANOUT=1 RUST_LOG=info,cosmic_settings_config=off; "
        "unset DISPLAY WAYLAND_DISPLAY; "
        f"cosmic-comp --no-xwayland 2>&1 & sleep {WAIT}; "
        "echo ===BUSD-LOG===; cat /tmp/busd.log; echo ===BUSD-END===")
    total = WAIT + 16
    log(f"--- busd + cosmic-comp (~{total}s) ---")
    out = d("cmd", compound, str(total), t=total + 40)
    log("--- output ---"); log(out.replace("\r","")[-2000:])
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m5f-busd-{ARCH}-serial.log")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5f BUSD DONE ====")
if __name__ == "__main__": main()
