#!/usr/bin/env python3
# M7b matrix: one boot, run m7repro across {A,B}x{ct,mt}, capture each ring dump.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
COMBOS = sys.argv[3].split(",") if len(sys.argv) > 3 else ["Act","Amt","Bct","Bmt"]

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def main():
    log(f"==== M7b matrix {ARCH} {MODE} {COMBOS} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    for combo in COMBOS:
        variant, flavor = combo[0], combo[1:]
        marker = f"M7BX-{combo}"
        d("cmd", f"rm -f /tmp/m7repro.sock; echo {marker}", t=8)
        log(f"\n######## RUN {combo} ########")
        out = d("cmd", f"/bin/m7repro {variant} {flavor}", t=45)
        # verdict from markers
        verdict = "FAIL(no-success)"
        if "SUCCESS" in out or "R:DONE" in out or "B:DONE" in out: verdict = "PASS"
        if "WATCHDOG" in out: verdict = "FAIL(watchdog)"
        log(f"[{combo}] verdict={verdict}")
        # extract serial window since marker
        try:
            with open(SERIAL_LOG, "r", errors="replace") as f: data = f.read()
            idx = data.rfind(marker)
            window = data[idx:] if idx >= 0 else data[-30000:]
            dst = f"{OUT}/m7b-mx-{ARCH}-{combo}.log"
            with open(dst, "w") as g: g.write(window)
            nmark = window.count("MARK 6")
            log(f"[{combo}] serial->{dst} ({len(window)}B) MARK6seen={ 'yes' if 'MARK 6' in window or 'POLLED' in window else 'NO'}")
        except Exception as e:
            log(f"[{combo}] serial err {e}")
    clean(); log("==== matrix DONE ====")

if __name__ == "__main__": main()
