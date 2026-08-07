#!/usr/bin/env python3
# M5 diagnostic: isolate the comprun failure. One boot.
#  1. busrun          -> does dbus-run-session + busd work at all?
#  2. comprun_nodbus  -> does cosmic-comp render on KMS WITHOUT the session bus?
# Dumps /tmp/cosmic.log (no grep on-image; tail+cut only). Screenshot after.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
WAIT = int(sys.argv[3]) if len(sys.argv) > 3 else 50
def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def dcmd(c, t=40):
    o = d("cmd", c, t=t); log(f"\n$ {c}\n{o.strip()[-2000:]}"); return o
def boot():
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ({ARCH} {MODE}) ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
    return False
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M5 DIAG {ARCH} {MODE} wait={WAIT} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-160:])
    log("\n########## 1. BUSRUN (dbus-run-session + busd isolation) ##########")
    dcmd("brush /bin/busrun", t=45)
    log("\n########## 2. COMPRUN_NODBUS (cosmic-comp on KMS, no session bus) ##########")
    dcmd("brush /bin/comprun_nodbus &", t=8)
    dcmd(f"sleep {WAIT}; echo SLEPT", t=WAIT+15)
    d("screenshot", f"{OUT}/m5-diag-nodbus-{ARCH}.ppm", t=30); log("[shot] diag-nodbus")
    dcmd("echo ===NODBUS-COSMIC-LOG===; tail -n 160 /tmp/cosmic.log | cut -c1-118", t=40)
    clean()
    log("==== M5 DIAG DONE ====")
if __name__ == "__main__": main()
