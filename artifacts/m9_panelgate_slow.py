#!/usr/bin/env python3
# m9: does cosmic-panel's render gate (is_dirty && has_frame) ever reopen after
# frame 1, and is it re-openable by waking the compositor with pointer motion?
#
# RUST_LOG scopes tracing to the one trace! in PanelSpace::render ("Rendering
# space"), which fires exactly once per successful panel render. No COSMIC
# source is patched.
#
# Timeline: idle -> jiggle window -> idle. Screenshots bracket each phase, so a
# clock that only advances while the pointer moves proves frame-callback
# starvation (has_frame), and one that never advances points at is_dirty or a
# stuck panel loop.
import subprocess, sys, os, time, threading, re, socket, json

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP = "/tmp/leandros-qmp.sock"
SERIAL = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m9-panelgate")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "m9b"
DRAIN = int(sys.argv[3]) if len(sys.argv) > 3 else 200
RUSTLOG = sys.argv[4] if len(sys.argv) > 4 else "info,cosmic_panel_bin::space::render=trace"
JIGGLE = (250, 290)     # wall seconds after session launch: pointer motion window
SHOTS = [200, 300, 340, 430]
W, H = (1280, 800) if ARCH == "aarch64" else (1920, 1080)
os.makedirs(OUT, exist_ok=True)


def d(*a, t=260, env=None):
    e = dict(os.environ); e.update(env or {})
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t, env=e)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def log(*a):
    print(*a, flush=True)


def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(2)


def qmp(cmds):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(5); s.connect(QMP)
        f = s.makefile("rwb"); f.readline()
        f.write(b'{"execute":"qmp_capabilities"}\n'); f.flush(); f.readline()
        ok = True
        for c in cmds:
            f.write((json.dumps(c) + "\n").encode()); f.flush()
            ok = ok and "return" in f.readline().decode(errors="replace")
        s.close(); return ok
    except Exception:
        return False


def mouse(x, y):
    ev = lambda ax, v: {"execute": "input-send-event",
                        "arguments": {"events": [{"type": "abs", "data": {"axis": ax, "value": v}}]}}
    return qmp([ev("x", int(x * 0x7fff / W)), ev("y", int(y * 0x7fff / H))])


def readppm(p):
    try:
        data = open(p, "rb").read()
    except OSError:
        return None
    if not data.startswith(b"P6"):
        return None
    idx = 2; f = []
    while len(f) < 3:
        while idx < len(data) and data[idx:idx + 1].isspace():
            idx += 1
        s = idx
        while idx < len(data) and not data[idx:idx + 1].isspace():
            idx += 1
        f.append(int(data[s:idx]))
    w, h, _ = f; idx += 1
    return (w, h, data[idx:])


def bar(p):
    """The panel bar strip only (rows 0..39), so the moving cursor elsewhere
    cannot masquerade as a ticking clock."""
    w, h, px = p
    return px[:w * 40 * 3]


def main():
    log(f"==== m9 panel-gate {ARCH} tag={TAG} drain={DRAIN} RUST_LOG={RUSTLOG} {time.ctime()} ====")
    try:
        os.remove(SERIAL)
    except OSError:
        pass
    env = {"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}
    booted = False; out = ""
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, "uefi", t=220, env=env)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("NO BOOT"); log(out[-2000:]); clean(); return
    d("login", "root", "root", t=45)
    cmd = f"RUST_LOG='{RUSTLOG}' sh /bin/start-cosmic-leandros &"
    threading.Thread(target=lambda: d("session", str(DRAIN), cmd, t=DRAIN + 40), daemon=True).start()
    log(f"[session launched; draining {DRAIN}s] cmd={cmd}")

    t0 = time.time()
    stop_jiggle = threading.Event()

    def jiggler():
        while time.time() - t0 < JIGGLE[0]:
            time.sleep(0.5)
        log(f"[t={int(time.time()-t0)}] pointer motion START")
        i = 0
        while time.time() - t0 < JIGGLE[1] and not stop_jiggle.is_set():
            i += 1
            mouse(200 + (i * 37) % 800, 300 + (i * 53) % 400)
            time.sleep(0.05)
        log(f"[t={int(time.time()-t0)}] pointer motion STOP after {i} moves")

    threading.Thread(target=jiggler, daemon=True).start()

    shots = []
    for when in SHOTS:
        dt = when - (time.time() - t0)
        if dt > 0:
            time.sleep(dt)
        ppm = f"{OUT}/{TAG}-{ARCH}-t{when}.ppm"
        d("screenshot", ppm, t=40)
        shots.append((when, ppm))
        log(f"[t={when:3d}] shot {'ok' if readppm(ppm) else 'FAILED'}")
    stop_jiggle.set()

    log("--- BAR-STRIP (rows 0-39) BYTE COMPARE ---")
    imgs = [(w, readppm(p)) for w, p in shots]
    for i in range(len(imgs) - 1):
        (wa, a), (wb, b) = imgs[i], imgs[i + 1]
        if a and b:
            log(f"  t{wa} vs t{wb}: bar identical={bar(a)==bar(b)}  whole-screen identical={a[2]==b[2]}")

    time.sleep(4)
    try:
        data = open(SERIAL, errors="replace").read()
        ct = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', data))
        open(f"{OUT}/{TAG}-{ARCH}-serial.txt", "w").write(ct)
        rend = [l for l in ct.splitlines() if "Rendering space" in l]
        open(f"{OUT}/{TAG}-{ARCH}-renders.txt", "w").write("\n".join(rend))
        log(f"--- 'Rendering space' (panel render count) = {len(rend)} ---")
        for l in rend[:8]:
            log("  " + l.strip()[:140])
        if len(rend) > 16:
            log("  ...")
        for l in rend[-8:]:
            log("  " + l.strip()[:140])
        log("--- signals ---")
        for k in ("committed 220x32", "entering event loop", "Broken pipe", "PANEL MAIN ERR",
                  "panicked", "EL0 Fault", "Out of memory", "Waiting for configure",
                  "root layer shell surface removed", "Failed to submit rendering"):
            log(f"  '{k}' x{ct.count(k)}")
        ts = re.findall(r"^1970-01-01T(\d\d:\d\d:\d\d\.\d+)Z", ct, re.M)
        log(f"  guest-log first ts={ts[0] if ts else '-'} last ts={ts[-1] if ts else '-'} (lines={len(ts)})")
    except Exception as e:
        log(f"[serial err] {e}")
    clean()
    log("==== m9 panel-gate DONE ====")


if __name__ == "__main__":
    main()
