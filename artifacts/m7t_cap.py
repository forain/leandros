#!/usr/bin/env python3
# M7t: garble-proof capture. Boot aarch64 uefi-hvf, login, launch COSMIC in the
# background, then hold a PERSISTENT serial drainer for the whole settle window
# while the gated kernel serial-dump facility streams /tmp/panel.ckpt +
# panel.panic + panel.log out the PL011 TX every 6s. Screenshots go over the
# separate monitor socket, so they don't contend with the serial drainer. No
# post-launch keystrokes => no RX-starvation garble.
import subprocess, sys, os, time, shutil, re, socket
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7t-logs")
SERIAL_SOCK = "/tmp/leandros-serial.sock"
ARCH = "aarch64"
MODE = sys.argv[1] if len(sys.argv) > 1 else "uefi-hvf"
CWAIT = int(sys.argv[2]) if len(sys.argv) > 2 else 60
TAG = sys.argv[3] if len(sys.argv) > 3 else "cap"
RUSTLOG = sys.argv[4] if len(sys.argv) > 4 else "warn,cosmic_panel_bin=debug"
CAPFILE = f"{OUT}/m7t-{ARCH}-{TAG}-serial.log"
def d(*a, t=220):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True); time.sleep(2)

def drain(capfile, dur):
    """Persistent serial drainer: hold the socket, append everything, for dur s."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(4)
    try: s.connect(SERIAL_SOCK)
    except Exception as e:
        log(f"[drain] connect fail: {e}"); return
    s.settimeout(1.0); end = time.time() + dur; n = 0
    with open(capfile, "ab") as f:
        while time.time() < end:
            try:
                b = s.recv(65536)
                if b: f.write(b); f.flush(); n += len(b)
            except socket.timeout: pass
            except Exception: break
    try: s.close()
    except Exception: pass
    log(f"[drain] {n} bytes -> {capfile}")

def main():
    os.makedirs(OUT, exist_ok=True)
    open(CAPFILE, "wb").close()
    log(f"==== M7t cap {ARCH} {MODE} cwait={CWAIT} rustlog={RUSTLOG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 4):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready")):
            booted = True; break
    if not booted:
        log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    d("cmd", "export XDG_RUNTIME_DIR=/run/user/0", "6")
    d("cmd", "rm -f /tmp/panel.panic /tmp/panel.ckpt /tmp/panel.log", "5")
    d("cmd", f"export RUST_LOG={RUSTLOG}", "5")
    log(f"[launcher BACKGROUND, persistent drain {CWAIT}s]")
    d("cmd", "sh /bin/start-cosmic-leandros >/tmp/cs.log 2>&1 &", "8")
    # Screenshots via monitor socket, interleaved with drain windows.
    step = 15
    for k in range((CWAIT // step)):
        drain(CAPFILE, step)                       # holds serial for `step` s
        el = (k + 1) * step
        d("screenshot", f"{OUT}/m7t-{ARCH}-{TAG}-t{el}.ppm", t=30)  # monitor sock
        log(f"  ... {el}s [shot]")
    try:
        for ppm in os.listdir(OUT):
            if ppm.endswith(".ppm") and TAG in ppm:
                png = os.path.join(OUT, ppm[:-4] + ".png")
                subprocess.run(["sips", "-s", "format", "png", os.path.join(OUT, ppm), "--out", png],
                               capture_output=True)
    except Exception as e:
        log(f"[png err] {e}")
    log(f"[serial saved] {CAPFILE}")
    clean(); log("==== cap DONE ====")
if __name__ == "__main__":
    main()
