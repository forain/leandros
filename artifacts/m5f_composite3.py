#!/usr/bin/env python3
# M5f: robust composite. Send the WHOLE choreography as ONE compound command
# while the shell is idle (fully received before cosmic-comp loads the CPU and
# starves the UART). Run it non-blocking (Popen) and screenshot concurrently
# via QMP (CPU-independent). The compound self-reports ls + wl.log to serial.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
CSET = int(sys.argv[3]) if len(sys.argv) > 3 else 22   # cosmic settle before wlclient
WSET = int(sys.argv[4]) if len(sys.argv) > 4 else 26   # wlclient paint window
WLDISP = sys.argv[5] if len(sys.argv) > 5 else "wayland-1"
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
    log(f"==== M5f COMPOSITE3 {ARCH} {MODE} cset={CSET} wset={WSET} disp={WLDISP} {time.ctime()} ====")
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
    env = ("export XDG_RUNTIME_DIR=/run/user/0 COSMIC_BACKEND=kms "
           "COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1 SMITHAY_USE_LEGACY=1 "
           "ICED_BACKEND=tiny-skia COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1 RUST_LOG=info")
    d("cmd", env, "6", t=15)
    d("cmd", "unset DISPLAY WAYLAND_DISPLAY", "5", t=15)
    # ONE compound line, typed at idle shell, fully parsed before cosmic loads CPU.
    compound = (f"cosmic-comp --no-xwayland >/dev/null 2>&1 & sleep {CSET}; "
                f"echo M5F-RUNTIME; ls -la /run/user/0; "
                f"export WAYLAND_DISPLAY={WLDISP}; wlclient >/root/wl.log 2>&1 & "
                f"sleep {WSET}; echo M5F-WLLOG; cat /root/wl.log; echo M5F-GO-END")
    total = CSET + WSET + 12
    log(f"--- compound (non-blocking, ~{total}s) ---")
    proc = subprocess.Popen(["python3", DRIVER, "cmd", compound, str(total)],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    # screenshot mid wlclient-paint window
    time.sleep(CSET + max(6, WSET // 2))
    d("screenshot", f"{OUT}/m5f-composite3-{ARCH}.ppm", t=30); log("[shot1]")
    time.sleep(6)
    d("screenshot", f"{OUT}/m5f-composite3b-{ARCH}.ppm", t=30); log("[shot2]")
    try: out, _ = proc.communicate(timeout=total + 30)
    except subprocess.TimeoutExpired: proc.kill(); out = "(compound TIMEOUT)"
    log("--- compound output ---"); log((out or "")[-1200:])
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m5f-composite3-{ARCH}-serial.log")
    except Exception as e: log(f"[save err] {e}")
    clean()
    log("==== M5f COMPOSITE3 DONE ====")
if __name__ == "__main__": main()
