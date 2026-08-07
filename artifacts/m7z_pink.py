#!/usr/bin/env python3
# M7z pink-rect repro. Boot aarch64 (12:15 image), login root, force a SHORT
# cosmic-idle screen_off_time so the idle fade fires in seconds (default is
# 15 min), launch the COSMIC session with a persistent serial drainer,
# screenshot at intervals, sample RGB, and mid-run inject pointer+key via QMP
# to test whether input recovers the desktop.
import subprocess, sys, os, time, threading, re, socket, json, struct

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"
QMP_SOCK = "/tmp/leandros-qmp.sock"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7z-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
IDLE_MS = sys.argv[2] if len(sys.argv) > 2 else "4000"   # short idle for fast fade; "default"=don't inject
DRAIN = int(sys.argv[3]) if len(sys.argv) > 3 else 150
TAG = sys.argv[4] if len(sys.argv) > 4 else "m0"
SHOTS = [20, 30, 40, 55, 70, 90, 110, 130]
INJECT_AT = 95   # inject pointer+key around here
os.makedirs(OUT, exist_ok=True)

def d(*a, t=260, env=None):
    e = dict(os.environ); e.update(env or {})
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t, env=e)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"

def log(*a): print(*a, flush=True)

def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)

def sample_ppm(path):
    """Return list of (name,(r,g,b)) sampled points of a P6 PPM."""
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return None
    if not data.startswith(b"P6"):
        return None
    # parse header: P6 <w> <h> <maxval> then binary
    idx = 2; fields = []
    while len(fields) < 3:
        while idx < len(data) and data[idx:idx+1].isspace(): idx += 1
        if idx < len(data) and data[idx:idx+1] == b"#":
            while idx < len(data) and data[idx:idx+1] != b"\n": idx += 1
            continue
        start = idx
        while idx < len(data) and not data[idx:idx+1].isspace(): idx += 1
        fields.append(int(data[start:idx]))
    w, h, mx = fields
    idx += 1  # single whitespace after maxval
    pix = data[idx:]
    def at(x, y):
        x = max(0, min(w-1, x)); y = max(0, min(h-1, y))
        o = (y*w + x)*3
        return (pix[o], pix[o+1], pix[o+2])
    pts = {
        "center": at(w//2, h//2),
        "top_panel": at(w//2, 16),
        "tl": at(40, 40),
        "bl": at(40, h-40),
        "tr": at(w-40, 40),
        "wallpaper_lower": at(w//2, int(h*0.75)),
    }
    return (w, h, pts)

def qmp(cmds):
    """Connect to QMP, negotiate, run each command dict, return replies."""
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(5); s.connect(QMP_SOCK)
        f = s.makefile("rwb")
        f.readline()  # greeting
        f.write(b'{"execute":"qmp_capabilities"}\n'); f.flush(); f.readline()
        out = []
        for c in cmds:
            f.write((json.dumps(c) + "\n").encode()); f.flush()
            out.append(f.readline().decode(errors="replace").strip())
        s.close()
        return out
    except Exception as e:
        return [f"(QMP err {e})"]

def inject_input():
    # abs pointer move to two positions (motion) + a keypress, via QMP tablet.
    ev = lambda x, y: {"execute":"input-send-event","arguments":{"events":[
        {"type":"abs","data":{"axis":"x","value":x}},
        {"type":"abs","data":{"axis":"y","value":y}}]}}
    replies = qmp([ev(0x2000,0x2000), ev(0x5000,0x5000), ev(0x4000,0x4000)])
    # also a key via HMP monitor
    d("_hmp_sendkey", t=10) if False else None
    return replies

def main():
    log(f"==== M7z pink repro {ARCH} idle={IDLE_MS}ms drain={DRAIN} tag={TAG} {time.ctime()} ====")
    try: os.remove(SERIAL_LOG)
    except OSError: pass
    for f in os.listdir(OUT):
        if f.startswith(f"m7z-{ARCH}-{TAG}-"):
            try: os.remove(os.path.join(OUT, f))
            except OSError: pass
    env = {"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP_SOCK},server,nowait"}

    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, "uefi", t=220, env=env)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("no boot"); log(out[-2000:]); clean(); return
    log("[login root]"); d("login", "root", "root", t=45)

    if IDLE_MS != "default":
        cfgdir = "/root/.config/cosmic/com.system76.CosmicIdle/v1"
        setup = [
            f"mkdir -p {cfgdir}",
            f"printf 'Some({IDLE_MS})' > {cfgdir}/screen_off_time",
            f"wc -c {cfgdir}/screen_off_time",
            f"cat {cfgdir}/screen_off_time",
            "echo CFGDONE",
        ]
        r = d("session", "5", *setup, t=60)
        log("[cfg inject]", " ".join(x for x in r.split() if "CFGDONE" in x or "screen_off_time" in x)[:200])
        log("[cfg raw tail]", r[-300:].replace("\n", " "))

    def drainer():
        d("session", str(DRAIN), "sh /bin/start-cosmic-leandros &", t=DRAIN + 40)
    th = threading.Thread(target=drainer, daemon=True); th.start()
    log(f"[session launched; draining {DRAIN}s]")

    t0 = time.time()
    injected = False
    for when in SHOTS:
        if not injected and when >= INJECT_AT:
            log(f"[t~{when}] INJECT pointer via QMP:", inject_input())
            injected = True
        dt = when - (time.time() - t0)
        if dt > 0: time.sleep(dt)
        ppm = f"{OUT}/m7z-{ARCH}-{TAG}-t{when}.ppm"
        d("screenshot", ppm, t=40)
        s = sample_ppm(ppm)
        if s:
            w, h, pts = s
            log(f"[t={when:3d}s] {w}x{h} " + " ".join(f"{k}={v}" for k, v in pts.items()))
        else:
            log(f"[t={when:3d}s] (no ppm)")

    th.join(timeout=DRAIN + 50)
    try:
        with open(SERIAL_LOG, "r", errors="replace") as f:
            data = f.read()
        clean_txt = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', data))
        open(f"{OUT}/m7z-{ARCH}-{TAG}-serial.txt", "w").write(clean_txt[-1500000:])
        for key in ("cosmic-idle", "idle", "Idled", "Resumed", "fade", "single_pixel",
                    "single-pixel", "SinglePixel", "output_power", "OutputPower", "layer",
                    "Overlay", "power", "loginctl", "lock", "greeter", "locker",
                    "GL Renderer", "softpipe", "far=", "EL0 Fault", "panic", "Unknown id",
                    "Broken pipe", "PANEL MAIN ERR", "leandros-applet", "committed"):
            n = clean_txt.count(key)
            if n: log(f"  serial: '{key}' x{n}")
    except Exception as e:
        log(f"[serial err] {e}")
    clean()
    log("==== M7z run DONE ====")

if __name__ == "__main__":
    main()
