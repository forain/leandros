#!/usr/bin/env python3
# M6d W2 DIAGNOSTIC: run cosmic-comp + wlclient (no busd) with a W2DIAG-instrumented
# libgallium (softpipe) already staged into the image. Capture the FULL comp.log so
# every "W2DIAG ..." line (res_create / create_surface / res_from_handle /
# set_surface RENDER-TO-BUFFER) is preserved, naming the PIPE_BUFFER RT + its creator.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "d0"
CSET = int(sys.argv[4]) if len(sys.argv) > 4 else 30
WSET = int(sys.argv[5]) if len(sys.argv) > 5 else 25
LINES = [
    "export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia RUST_LOG=info",
    "export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1",
    "export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
    # stdout->file, but stderr stays on the INHERITED SERIAL so W2DIAG (fprintf
    # stderr) + comp tracing stream to /tmp/leandros-serial.log as CLEAN program
    # output (like the kernel [FAULT] lines) — immune to fb/CPU input-echo garble.
    "cosmic-comp --no-xwayland >/tmp/comp.log &",
    "COMP=$!",
    f"sleep {CSET}",
    "echo RT; ls -la /run/user/0",
    "export WAYLAND_DISPLAY=wayland-1",
    "wlclient >/tmp/wl.log 2>&1 &",
    "WL=$!",
    f"sleep {WSET}",
    # Kill comp+client via captured PID VARIABLES ($COMP/$WL — no command
    # substitution, no job spec) to release the fb + free CPU (pkill absent),
    # so the post-run serial cat of comp.log is not garbled/starved by comp.
    "kill -9 $WL",
    "kill -9 $COMP",
    "sleep 4",
    "echo ==FBRELEASED==",
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
    d("screenshot", f"{OUT}/m6d-{ARCH}-{TAG}-{name}.ppm", t=30); log(f"[shot {name}]")
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6d W2 DIAG {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); return
    d("login", "root", "root", t=45)
    d("cmd", "rm -f /tmp/w.sh; echo START", "8")
    for ln in LINES:
        d("cmd", f"echo '{ln}' >> /tmp/w.sh", "8")
    d("cmd", "wc -l /tmp/w.sh", "8")
    d("cmd", "/bin/sh /tmp/w.sh >/tmp/w.log 2>&1 & echo W2-LAUNCHED", "12")
    time.sleep(CSET + WSET // 2); shot("mid")
    time.sleep(WSET // 2 + 8); shot("end")
    # The script itself kills comp+client by PID at its tail; give it time to run.
    time.sleep(10)
    # fb should be released now -> clean serial. Dump comp.log (has W2DIAG lines).
    d("cmd", "echo ==WL==; cat /tmp/wl.log; echo ==WLEND==", "14")
    d("cmd", "echo ==W2DIAG==; cat /tmp/comp.log; echo ==COMPEND==", "40")
    d("cmd", "echo ==NEEDCK==; ls -la /usr/lib/libgallium-25.3.6.so", "8")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6d-{ARCH}-{TAG}-serial.log")
    except Exception as e: log(f"[serial err] {e}")
    clean()
    log("==== M6d W2 DIAG DONE ====")
if __name__ == "__main__": main()
