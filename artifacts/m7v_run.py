#!/usr/bin/env python3
# M7v milestone run: boot, login, launch the full COSMIC session with a
# persistent serial drainer, screenshot the framebuffer at several settle
# points, and report id-636 / panel-render / broken-pipe markers from serial.
import subprocess, sys, os, time, threading, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7v-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
TAG  = sys.argv[3] if len(sys.argv) > 3 else "m0"
DRAIN = int(sys.argv[4]) if len(sys.argv) > 4 else 150
SHOTS = [50, 75, 100, 130]

os.makedirs(OUT, exist_ok=True)

def d(*a, t=260):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"

def log(*a): print(*a, flush=True)

def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)

def main():
    log(f"==== M7v run {ARCH} {MODE} tag={TAG} {time.ctime()} ====")
    try: os.remove(SERIAL_LOG)
    except OSError: pass
    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("no boot"); log(out[-2000:]); clean(); return
    log("[login root]")
    d("login", "root", "root", t=45)

    def drainer():
        d("session", str(DRAIN), "sh /bin/start-cosmic-leandros &", t=DRAIN + 40)
    th = threading.Thread(target=drainer, daemon=True); th.start()
    log(f"[session launched; draining {DRAIN}s]")

    t0 = time.time()
    for when in SHOTS:
        dt = when - (time.time() - t0)
        if dt > 0: time.sleep(dt)
        ppm = f"{OUT}/m7v-{ARCH}-{TAG}-t{when}.ppm"
        r = d("screenshot", ppm, t=40)
        sz = os.path.getsize(ppm) if os.path.exists(ppm) else 0
        log(f"[shot t={when}s -> {ppm} ({sz} B)] {r.strip()[:100]}")

    th.join(timeout=DRAIN + 50)
    try:
        with open(SERIAL_LOG, "r", errors="replace") as f:
            data = f.read()
        clean_txt = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', data))
        open(f"{OUT}/m7v-{ARCH}-{TAG}-serial.txt", "w").write(clean_txt[-600000:])
        for key in ("Unknown id", "Broken pipe", "invalid_object", "PANEL MAIN ERR",
                    "cosmic-panel", "GL Renderer", "softpipe", "far=", "EL0 Fault",
                    "panic", "[UCK", "Initializing OpenGL"):
            n = clean_txt.count(key)
            if n: log(f"  serial: '{key}' x{n}")
    except Exception as e:
        log(f"[serial err] {e}")
    clean()
    log("==== run DONE ====")

if __name__ == "__main__":
    main()
