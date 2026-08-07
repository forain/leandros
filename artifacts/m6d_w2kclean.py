#!/usr/bin/env python3
# M6d W2 KERNEL-TRACE clean capture. The crash is at comp's OWN output-RT setup
# (dmabuf import), independent of any client. So: NO wlclient. Launch comp with
# an inline trailing `sleep` in the SAME command, so the driver sends exactly ONE
# line (before comp starves) and then only READS for the whole window — no
# concurrent driver input to garble/truncate the clean kernel W2K serial output.
import subprocess, sys, os, time, shutil
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
SERIAL_LOG = "/tmp/leandros-serial.log"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
TAG = sys.argv[3] if len(sys.argv) > 3 else "k0"
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
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M6d W2K CLEAN {ARCH} {MODE} {TAG} {time.ctime()} ====")
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
    # ONE send: comp + inline sleep. Driver then only READS (no more input) for ~72s.
    log("[single-send comp+sleep reader window]")
    d("cmd", "cosmic-comp --no-xwayland >/tmp/comp.log & sleep 68; echo W2KWINDOWEND", "80")
    try: shutil.copy(SERIAL_LOG, f"{OUT}/m6d-{ARCH}-{TAG}-serial.log")
    except Exception as ex: log(f"[serial err] {ex}")
    clean()
    log("==== M6d W2K CLEAN DONE ====")
if __name__ == "__main__": main()
