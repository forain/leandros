#!/usr/bin/env python3
# M9 item-9 FB_DAMAGE_CLIPS diagnostic.
#
# Differences from m8_cursor.py (which cannot be used as-is):
#   * m8's DRMSTAT regex is positional and predates dmg_*/blobs/evpush, so it
#     silently returns 0 for curs_up/curs_mv/atomic on the new line. This
#     parser is order-independent key=0xHEX.
#   * m8 picks its "busiest window" by curs_mv delta. That is the quantity
#     under test. Window selection is keyed on `evpush` here — the guest-side
#     witness that injected input actually reached the kernel ring.
#
# usage: m9_damage.py [arch] [tag]
import subprocess, sys, os, time, threading, re, socket, json, math

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP = "/tmp/leandros-qmp.sock"
SERIAL = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m9-fb-damage-clips")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "dmg"
W, H = (1280, 800) if ARCH == "aarch64" else (1920, 1080)
SESSION_CMD = "sh /bin/start-cosmic-leandros &"

SETTLE = 115          # s before the burst
BURST_LEN = 75        # s of continuous 60/s motion (spec: >= 60)
DRAIN = 260           # s the session thread keeps the serial reader alive
os.makedirs(OUT, exist_ok=True)
SHOTS = os.path.join(OUT, "shots")
os.makedirs(SHOTS, exist_ok=True)


def d(*a, t=260, env=None):
    e = dict(os.environ); e.update(env or {})
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True,
                           text=True, timeout=t, env=e)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def log(*a):
    print(*a, flush=True)


def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)


class Qmp:
    def __init__(self):
        self.f = None
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(5); s.connect(QMP)
            self.s = s; self.f = s.makefile("rwb")
            self.f.readline()
            self.f.write(b'{"execute":"qmp_capabilities"}\n'); self.f.flush()
            self.f.readline()
        except Exception as e:
            log(f"[qmp] connect failed: {e}")
            self.f = None

    def move(self, x, y):
        if not self.f:
            return False
        ok = True
        for axis, val, span in (("x", x, W), ("y", y, H)):
            ev = {"execute": "input-send-event", "arguments": {"events": [
                {"type": "abs", "data": {"axis": axis,
                                         "value": int(val * 0x7fff / span)}}]}}
            try:
                self.f.write((json.dumps(ev) + "\n").encode()); self.f.flush()
                resp = self.f.readline().decode(errors="replace")
                if "return" not in resp:
                    ok = False
                    if not hasattr(self, "_warned"):
                        log(f"[qmp] input-send-event rejected: {resp.strip()[:200]}")
                        self._warned = True
            except Exception:
                self.f = None
                return False
        return ok

    def screendump(self, path):
        if not self.f:
            return False
        try:
            self.f.write((json.dumps({"execute": "screendump",
                                      "arguments": {"filename": path}}) + "\n").encode())
            self.f.flush(); self.f.readline()
            return True
        except Exception:
            return False


KV = re.compile(r"([a-z_]+)=0x([0-9A-Fa-f]+)")


def parse_stats(text):
    """Order-independent parse of every [DRMSTAT] line."""
    out = []
    for line in text.splitlines():
        i = line.find("[DRMSTAT]")
        if i < 0:
            continue
        rec = {k: int(v, 16) for k, v in KV.findall(line[i:])}
        if "t" in rec:
            rec["_raw"] = line[i:].strip()
            out.append(rec)
    return out


def g(s, k):
    return s.get(k, 0)


def read_ppm(path):
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return None
    if not data.startswith(b"P6"):
        return None
    # header: P6 <w> <h> <maxval>, whitespace/comment separated
    idx, fields = 2, []
    while len(fields) < 3 and idx < len(data):
        while idx < len(data) and data[idx:idx + 1].isspace():
            idx += 1
        if data[idx:idx + 1] == b"#":
            while idx < len(data) and data[idx] != 0x0A:
                idx += 1
            continue
        st = idx
        while idx < len(data) and not data[idx:idx + 1].isspace():
            idx += 1
        fields.append(int(data[st:idx]))
    idx += 1
    w, h, _mx = fields
    return (w, h, data[idx:idx + w * h * 3])


def diff_ppm(a, b):
    """Return (n_differing_pixels, bbox) or None."""
    A, B = read_ppm(a), read_ppm(b)
    if A is None or B is None or A[0] != B[0] or A[1] != B[1]:
        return None
    w, h, pa = A
    _, _, pb = B
    if len(pa) != len(pb):
        return None
    if pa == pb:
        return (0, None, w, h)
    n = 0
    x0, y0, x1, y1 = w, h, -1, -1
    for y in range(h):
        ro = y * w * 3
        ra = pa[ro:ro + w * 3]
        rb = pb[ro:ro + w * 3]
        if ra == rb:
            continue
        for x in range(w):
            o = x * 3
            if ra[o:o + 3] != rb[o:o + 3]:
                n += 1
                if x < x0: x0 = x
                if x > x1: x1 = x
                if y < y0: y0 = y
                if y > y1: y1 = y
    return (n, (x0, y0, x1, y1), w, h)


def main():
    log(f"==== M9 FB_DAMAGE_CLIPS diag {ARCH} tag={TAG} {time.ctime()} ====")
    try:
        os.remove(SERIAL)
    except OSError:
        pass
    env = {"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}

    booted, out = False, ""
    for attempt in (1, 2):
        log(f"#### BOOT {attempt} ####")
        clean()
        out = d("start", ARCH, "uefi", t=220, env=env)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True
            break
    if not booted:
        log("NO BOOT"); log(out[-1500:]); clean(); return 1

    d("login", "root", "root", t=45)
    threading.Thread(
        target=lambda: d("session", str(DRAIN), SESSION_CMD, t=DRAIN + 60),
        daemon=True).start()
    log(f"[session launched; serial reader alive {DRAIN}s]")
    t0 = time.time()

    def at(sec):
        time.sleep(max(0, sec - (time.time() - t0)))

    at(SETTLE - 15)
    q = Qmp()
    shots = {}

    def shot(name):
        p = f"{SHOTS}/{TAG}-{name}.ppm"
        q.screendump(p)
        shots[name] = p
        log(f"[t={int(time.time()-t0)}] shot {name} -> {p}")

    # --- quiet-period stale check pair (no pointer motion) ---
    shot("quiet1")
    at(SETTLE - 10)
    shot("quiet2")

    # --- pointer burst ---
    at(SETTLE)
    log(f"[t={int(time.time()-t0)}] BURST START ({BURST_LEN}s @ ~60 moves/s)")
    b0 = time.time()
    n = 0
    next_shot = [20, 45]
    while time.time() - b0 < BURST_LEN:
        p = time.time() - b0
        x = int(W * 0.5 + W * 0.35 * math.sin(p * 1.7))
        y = int(H * 0.5 + H * 0.30 * math.sin(p * 2.3))
        if q.move(x, y):
            n += 1
        if next_shot and p >= next_shot[0]:
            shot(f"burst{int(next_shot[0])}")
            next_shot.pop(0)
        time.sleep(1.0 / 60)
    bdur = time.time() - b0
    log(f"[t={int(time.time()-t0)}] BURST END: {n} moves in {bdur:.1f}s "
        f"= {n/bdur:.1f} moves/s")

    shot("post1")
    time.sleep(3.5)
    shot("post2")
    time.sleep(6)
    shot("post3")
    time.sleep(5)

    # --- report ---
    try:
        data = open(SERIAL, errors="replace").read()
    except OSError:
        log("no serial log"); clean(); return 1
    ct = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", re.sub(r"\x1b[=>78]", "", data))
    open(f"{OUT}/{TAG}-serial.txt", "w").write(ct[-2000000:])

    stats = parse_stats(ct)
    open(f"{OUT}/{TAG}-drmstat.txt", "w").write(
        "\n".join(s["_raw"] for s in stats) + "\n")
    log(f"\n---- {len(stats)} DRMSTAT samples ----")
    for s in stats:
        log(f"  t={s['t']/100:8.2f}s fsub={g(s,'flips_sub'):6d} fdel={g(s,'flips_del'):6d} "
            f"full={g(s,'dmg_full'):6d} rect={g(s,'dmg_rect'):6d} skip={g(s,'dmg_skip'):6d} "
            f"px={g(s,'dmg_px'):12d} blobs={g(s,'blobs'):7d} "
            f"cup={g(s,'curs_up'):5d} cmv={g(s,'curs_mv'):6d} "
            f"atomic={g(s,'atomic'):6d} evpush={g(s,'evpush'):7d} "
            f"flip_us={g(s,'flip_us')}")

    # --- window selection keyed on evpush (NOT curs_mv) ---
    def window(stats, key, label):
        best = None
        for a, b in zip(stats, stats[1:]):
            dt = (b["t"] - a["t"]) / 100.0
            if dt <= 0:
                continue
            dk = g(b, key) - g(a, key)
            if best is None or dk > best[0]:
                best = (dk, a, b, dt)
        if not best:
            return None
        dk, a, b, dt = best
        log(f"\n---- busiest window by {key} ({label}, {dt:.1f}s) ----")
        return report(a, b, dt)

    def report(a, b, dt):
        dfull = g(b, 'dmg_full') - g(a, 'dmg_full')
        drect = g(b, 'dmg_rect') - g(a, 'dmg_rect')
        dskip = g(b, 'dmg_skip') - g(a, 'dmg_skip')
        datm = g(b, 'atomic') - g(a, 'atomic')
        dpx = g(b, 'dmg_px') - g(a, 'dmg_px')
        r = dict(dt=dt,
                 flips=(g(b, 'flips_sub') - g(a, 'flips_sub')) / dt,
                 deliv=(g(b, 'flips_del') - g(a, 'flips_del')) / dt,
                 cmv=(g(b, 'curs_mv') - g(a, 'curs_mv')) / dt,
                 cup=(g(b, 'curs_up') - g(a, 'curs_up')) / dt,
                 atomic=datm / dt,
                 evpush=(g(b, 'evpush') - g(a, 'evpush')) / dt,
                 full=dfull, rect=drect, skip=dskip, atm=datm, px=dpx)
        log(f"  flips/s     : {r['flips']:.2f}   (pre-patch baseline 6.0)")
        log(f"  delivered/s : {r['deliv']:.2f}")
        log(f"  cursor mv/s : {r['cmv']:.2f}   (control: must NOT fall vs 6.0)")
        log(f"  cursor up/s : {r['cup']:.2f}")
        log(f"  atomic/s    : {r['atomic']:.2f}")
        log(f"  evpush/s    : {r['evpush']:.2f}   (control: must be ~60)")
        log(f"  dmg full/rect/skip in window : {dfull} / {drect} / {dskip}")
        log(f"  SANITY full+rect+skip={dfull+drect+dskip} vs atomic={datm} "
            f"-> {'OK' if dfull+drect+dskip == datm else 'MISMATCH (delta %d)' % (datm-(dfull+drect+dskip))}")
        if drect:
            log(f"  dmg_px/dmg_rect = {dpx/drect:.0f} px  "
                f"= {100.0*dpx/drect/(W*H):.2f}% of {W}x{H} ({W*H} px)")
        else:
            log("  dmg_px/dmg_rect = n/a (no rect-path presents in window)")
        return r

    window(stats, "evpush", "guest input witness")
    window(stats, "atomic", "commit rate")

    # cumulative totals over the whole run
    if stats:
        a, b = stats[0], stats[-1]
        log("\n---- cumulative (first->last sample) ----")
        report(a, b, max(1e-9, (b["t"] - a["t"]) / 100.0))

    # --- stale-pixel / screendump comparisons ---
    log("\n---- screendump comparisons ----")
    pairs = [("quiet1", "quiet2"), ("burst20", "burst45"),
             ("post1", "post2"), ("post2", "post3"), ("quiet2", "post3")]
    for x, y in pairs:
        if x in shots and y in shots:
            r = diff_ppm(shots[x], shots[y])
            if r is None:
                log(f"  {x} vs {y}: UNREADABLE/size-mismatch")
            elif r[0] == 0:
                log(f"  {x} vs {y}: IDENTICAL ({r[2]}x{r[3]})")
            else:
                n, bb, w, h = r
                log(f"  {x} vs {y}: {n} px differ ({100.0*n/(w*h):.3f}%) bbox={bb}")

    for pat in ("panic", "PANIC", "Fault", "Broken pipe", "Unknown id",
                "damaged gpu.flush FAILED", "no VirtIO GPU"):
        hits = [l for l in ct.splitlines() if pat in l]
        if hits:
            log(f"\n[grep '{pat}'] {len(hits)} lines, last 6:")
            for l in hits[-6:]:
                log("   " + l[:200])

    clean()
    return 0


if __name__ == "__main__":
    sys.exit(main())
