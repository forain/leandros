#!/usr/bin/env python3
# m9 GAP2 re-take on the FIXED (softfloat) kernel, with a pointer-motion window.
#
# The 2026-08-03 GAP2 measurement was taken while the aarch64 FP/SIMD clobber was
# live, so it is re-taken here. The added motion window is the discriminator the
# original run lacked:
#
#   applet pool advances, panel bar pools CONSTANT even during motion
#       -> the panel never re-renders (its own render gate is shut)
#   applet pool advances, panel bar pools ADVANCE during motion but the clock
#   digits on screen do not
#       -> the panel re-renders but composites a stale applet texture
#
# Needs mm::gap2::ON = true in the kernel.
import subprocess, sys, os, time, threading, re, socket, json, itertools

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP = "/tmp/leandros-qmp.sock"
SERIAL = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m9-panelgate")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "m9g"
DRAIN = int(sys.argv[3]) if len(sys.argv) > 3 else 210
JIGGLE = (75, 110)
SHOTS = [65, 105, 150, 190]
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


def clockstrip(p, dst):
    """Rows 0..33, cols 500..790 — the applet's clock block only."""
    r = readppm(p)
    if not r:
        return None
    w, h, px = r
    out = bytearray()
    for y in range(0, 34):
        out += px[(y * w + 500) * 3:(y * w + 790) * 3]
    open(dst, "wb").write(b"P6\n290 34\n255\n" + bytes(out))
    return bytes(out)


def analyze_sums(raw):
    recs = {}
    for m in re.finditer(r"\[G2SUM\] t=0x([0-9a-f]+) idx=0x([0-9a-f]+) np=0x([0-9a-f]+) "
                         r"p0=0x([0-9a-f]+) vlen=0x([0-9a-f]+) sum=(0x[0-9a-f]+)", raw):
        recs.setdefault(int(m.group(2), 16), []).append(
            (int(m.group(1), 16), int(m.group(5), 16), m.group(6)))
    out = {}
    for idx, s in sorted(recs.items()):
        sums = [x[2] for x in s]
        # ordinal of every sample whose sum differs from its predecessor
        trans = [i for i in range(1, len(s)) if sums[i] != sums[i - 1]]
        runs = [(k, len(list(g))) for k, g in itertools.groupby(sums)]
        out[idx] = {"n": len(s), "vlen": s[0][1], "distinct": len(set(sums)),
                    "trans_ord": trans, "longest_static_run": max(r[1] for r in runs),
                    "t0": s[0][0], "t1": s[-1][0]}
    return out


def main():
    log(f"==== m9 GAP2 re-take {ARCH} tag={TAG} drain={DRAIN} jiggle={JIGGLE} {time.ctime()} ====")
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
    threading.Thread(target=lambda: d("session", str(DRAIN), "sh /bin/start-cosmic-leandros &",
                                      t=DRAIN + 40), daemon=True).start()
    log(f"[session launched; draining {DRAIN}s]")

    t0 = time.time()
    marks = {}

    def jiggler():
        while time.time() - t0 < JIGGLE[0]:
            time.sleep(0.5)
        marks["motion_start"] = time.time() - t0
        log(f"[t={int(marks['motion_start'])}] pointer motion START")
        i = 0
        while time.time() - t0 < JIGGLE[1]:
            i += 1
            mouse(200 + (i * 37) % 800, 300 + (i * 53) % 400)
            time.sleep(0.05)
        marks["motion_stop"] = time.time() - t0
        log(f"[t={int(marks['motion_stop'])}] pointer motion STOP after {i} moves")

    threading.Thread(target=jiggler, daemon=True).start()

    shots = []
    for when in SHOTS:
        dt = when - (time.time() - t0)
        if dt > 0:
            time.sleep(dt)
        ppm = f"{OUT}/{TAG}-{ARCH}-t{when}.ppm"
        d("screenshot", ppm, t=40)
        strip = clockstrip(ppm, f"{OUT}/clock-{TAG}-t{when}.ppm")
        shots.append((when, ppm, strip))
        log(f"[t={when:3d}] shot {'ok' if strip else 'FAILED'}")

    log("--- CLOCK STRIP (rows 0-33, cols 500-790) BYTE COMPARE ---")
    for i in range(len(shots) - 1):
        a, b = shots[i], shots[i + 1]
        if a[2] and b[2]:
            log(f"  t{a[0]} vs t{b[0]}: clock-strip identical={a[2]==b[2]}")
    base = shots[0][2]
    for when, _, strip in shots[1:]:
        if base and strip:
            log(f"  t{shots[0][0]} vs t{when}: clock-strip identical={base==strip}")

    time.sleep(4)
    try:
        data = open(SERIAL, errors="replace").read()
        ct = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', data))
        open(f"{OUT}/{TAG}-{ARCH}-serial.txt", "w").write(ct)
        g2 = [l for l in ct.splitlines() if "[G2" in l]
        open(f"{OUT}/{TAG}-{ARCH}-g2lines.txt", "w").write("\n".join(g2))
        log(f"--- G2 lines total={len(g2)} (SUM={sum('[G2SUM]' in l for l in g2)}) ---")
        for l in g2:
            if "[G2SUM]" in l:
                continue
            if "[G2FALL]" in l and "kind=0x4" in l:
                continue
            log("  " + l.strip()[:150])
        st = analyze_sums(ct)
        log("--- PER-POOL [G2SUM] (0.5 Hz sampler; ordinals are sample numbers) ---")
        log(f"    motion window ~ samples {marks.get('motion_start',0)/2:.0f}..{marks.get('motion_stop',0)/2:.0f}"
            f" (wall {marks.get('motion_start',0):.0f}s..{marks.get('motion_stop',0):.0f}s after launch)")
        for idx, v in st.items():
            verdict = "CONSTANT" if not v["trans_ord"] else f"ADVANCED ({len(v['trans_ord'])} transitions)"
            log(f"  idx=0x{idx:x} vlen=0x{v['vlen']:x} samples={v['n']} distinct={v['distinct']} "
                f"longest_static_run={v['longest_static_run']} -> {verdict}")
            if v["trans_ord"]:
                log(f"      change ordinals: {v['trans_ord'][:40]}{' ...' if len(v['trans_ord'])>40 else ''}")
        log("--- signals ---")
        for k in ("committed 220x32", "entering event loop", "Broken pipe", "PANEL MAIN ERR",
                  "panicked", "EL0 Fault", "Out of memory", "Failed to render",
                  "Waiting for configure"):
            log(f"  '{k}' x{ct.count(k)}")
    except Exception as e:
        log(f"[serial err] {e}")
    clean()
    log("==== m9 GAP2 re-take DONE ====")


if __name__ == "__main__":
    main()
