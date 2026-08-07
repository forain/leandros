#!/usr/bin/env python3
# M9 Stage 0a: does cosmic-comp advertise zwp_linux_dmabuf_v1 on our software
# EGL device? Boot, log in, run wl-globals BEFORE the session (control), launch
# the COSMIC session, run wl-globals again inside it, dump the raw serial.
#
# Deliberately NOT `driver.py cmd`: its prompt heuristic has swallowed lines on
# this project before. `driver.py session` prints the raw, unparsed transcript.
import subprocess, sys, os, time, threading, re

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m9-crossopen-dmabuf")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "r1"
STEP = int(sys.argv[3]) if len(sys.argv) > 3 else 60

TOKEN = "LANER_CONSOLE_ALIVE_9A7C"

# The dumper backgrounded from the login shell sleeps 100s, then dumps every
# wayland-* socket in $XDG_RUNTIME_DIR three times, 30s apart.
CMDS = [
    "/bin/wl-globals 0 1 0",                  # PRE-SESSION CONTROL: expect sockets=0
    "sh /bin/start-cosmic-leandros &",        # the session
    "/bin/wl-globals 100 3 30 &",             # dumps at t+100 / +130 / +160
    "true",
    "true",
    f"echo {TOKEN}",                          # console-alive control
    "/bin/wl-globals 0 2 15",                 # second, foreground measurement
]

os.makedirs(OUT, exist_ok=True)


def d(*a, t=260):
    try:
        r = subprocess.run(["python3", "-u", DRIVER, *a],
                           capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def log(*a):
    print(*a, flush=True)


def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)


def main():
    log(f"==== M9 Stage 0a {ARCH} tag={TAG} step={STEP}s {time.ctime()} ====")
    try:
        os.remove(SERIAL_LOG)
    except OSError:
        pass

    booted = False
    out = ""
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####")
        clean()
        out = d("start", ARCH, "uefi", t=220)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True
            break
    if not booted:
        log("NO BOOT")
        log(out[-2000:])
        clean()
        return

    log("[login root]")
    log(d("login", "root", "root", t=45)[-400:])

    total = STEP * len(CMDS)
    holder = {}

    def drainer():
        holder["t"] = d("session", str(STEP), *CMDS, t=total + 120)

    th = threading.Thread(target=drainer, daemon=True)
    th.start()
    log(f"[session cmds sent; total pump ~{total}s]")

    # Corroboration that the session really came up, not just that a socket did.
    t0 = time.time()
    for when in (150, 300):
        dt = when - (time.time() - t0)
        if dt > 0:
            time.sleep(dt)
        ppm = f"{OUT}/stage0a-{ARCH}-{TAG}-t{when}.ppm"
        d("screenshot", ppm, t=40)
        sz = os.path.getsize(ppm) if os.path.exists(ppm) else 0
        log(f"[shot t={when}s {sz} B]")

    th.join(timeout=total + 180)

    try:
        data = open(SERIAL_LOG, "r", errors="replace").read()
    except Exception as e:
        log(f"[serial read err] {e}")
        clean()
        return

    clean_txt = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", re.sub(r"\x1b[=>78]", "", data))
    raw_path = f"{OUT}/stage0a-{ARCH}-{TAG}-serial.txt"
    open(raw_path, "w").write(clean_txt)
    log(f"[raw serial -> {raw_path} ({len(clean_txt)} chars)]")

    log("---- [WLG] lines ----")
    for line in clean_txt.splitlines():
        if "[WLG]" in line:
            log("  " + line.strip())

    log("---- markers ----")
    for key in (TOKEN, "GL Renderer", "softpipe", "EGL", "is_software",
                "Failed to initialize hardware-acceleration", "cosmic-comp",
                "cosmic-panel", "leandros-applet", "EL0 Fault", "panic",
                "Rendering space", "wayland-"):
        n = clean_txt.count(key)
        log(f"  '{key}' x{n}")

    clean()
    log("==== DONE ====")


if __name__ == "__main__":
    main()
