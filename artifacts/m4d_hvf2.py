#!/usr/bin/env python3
# M4d decisive functional test (robust: all delivery via driver.py cmd blessed path).
# Boot <accel>, login, launch anvil, give it time, launch wlclient, then READ /tmp/wl.log
# (a file -> robust) for the roundtrip result. wl.log "roundtrip done"+"configured ->
# painted" == anvil services the client == SLOW-NOT-STUCK + M4 exit crit 1.
# Serial stays connected during the client window (short combined line) so kernel UXTR
# ACC/SND/RCV is captured too. Screenshots via monitor. arg1=accel (uefi-hvf/uefi-tcg),
# arg2=anvil_settle_seconds.
import subprocess, sys, os, time

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ACCEL = sys.argv[1] if len(sys.argv) > 1 else "uefi-hvf"
SETTLE = int(sys.argv[2]) if len(sys.argv) > 2 else 45
TAG = ACCEL.replace("uefi-", "")

def log(*a): print(*a, flush=True)

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT driver.py {' '.join(a)})"

def dcmd(c, t=12):
    out = d("cmd", c, t=t)
    log(f"$ {c}\n{out.strip()[-800:]}")
    return out

def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def boot():
    for attempt in range(1, 5):
        log(f"\n#### BOOT attempt {attempt} ({ACCEL}) ####")
        clean()
        os.environ["LEANDROS_QEMU_EXTRA"] = "-qmp unix:/tmp/leandros-qmp.sock,server,nowait"
        out = d("start", "aarch64", ACCEL, t=150)
        log(out[-400:])
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
        log("boot failed, retry")
    return False

def main():
    log(f"==== M4D HVF2 {ACCEL} settle={SETTLE} {time.ctime()} ====")
    if not boot():
        log("FATAL: no boot"); return
    log(d("login","root","root", t=45)[-200:])
    dcmd("mkdir -p /run/user/0")
    dcmd("export ANVIL_DRM_DEVICE=/dev/dri/card0")
    dcmd("export SMITHAY_USE_LEGACY=1")
    dcmd("export XDG_RUNTIME_DIR=/run/user/0")
    dcmd("echo RTDIR=[$XDG_RUNTIME_DIR]")
    log(f"\n---- launch anvil, settle {SETTLE}s ----")
    dcmd("anvil --tty-udev >/tmp/anvil.log 2>&1 &", t=8)
    t0 = time.time()
    time.sleep(SETTLE)
    dcmd("wc -l /tmp/anvil.log")
    dcmd("tail -n 6 /tmp/anvil.log", t=12)
    d("screenshot", f"/tmp/m4d-{TAG}-anvil.ppm", t=30)
    log(f"[screenshot] /tmp/m4d-{TAG}-anvil.ppm")
    log(f"\n---- launch wlclient (t+{time.time()-t0:.0f}s since anvil) ----")
    dcmd("export WAYLAND_DISPLAY=wayland-1")
    # keep serial connected through the client roundtrip so UXTR ACC/SND/RCV is captured
    dcmd("wlclient >/tmp/wl.log 2>&1 & sleep 20", t=28)
    log("\n---- DECISIVE: wl.log ----")
    dcmd("cat /tmp/wl.log", t=12)
    dcmd("tail -n 10 /tmp/anvil.log", t=12)
    d("screenshot", f"/tmp/m4d-{TAG}-client.ppm", t=30)
    log(f"[screenshot] /tmp/m4d-{TAG}-client.ppm")
    log("==== M4D HVF2 DONE ====")

if __name__ == "__main__":
    main()
