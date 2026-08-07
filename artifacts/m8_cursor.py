#!/usr/bin/env python3
# M8 cursor/atomic-KMS verification.
#
# Boots fresh, logs in as root, launches the full COSMIC session, then drives a
# sustained ~60 moves/s pointer burst while capturing [DRMSTAT] lines. Reports
# page flips/s and cursor-plane traffic during the burst window.
#
# Baseline to beat (M7z4 measurement, legacy KMS): 0.9 page flips/s under
# 60 pointer moves/s, and no cursor-plane traffic at all.
#
# usage: m8_cursor.py [arch] [tag] [drain] [burst_start,burst_len]
import subprocess, sys, os, time, threading, re, socket, json

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP = "/tmp/leandros-qmp.sock"
SERIAL = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m8-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "cursor"
DRAIN = int(sys.argv[3]) if len(sys.argv) > 3 else 200
BURST = sys.argv[4] if len(sys.argv) > 4 else "110,30"
BURST_AT, BURST_LEN = (int(x) for x in BURST.split(","))
W, H = (1280, 800) if ARCH == "aarch64" else (1920, 1080)
# LEGACY=1 in the environment runs the SAME build through smithay's legacy
# DRM backend. That is the control: if pointer motion produces no compositor
# response there either, the problem is input delivery, not atomic KMS.
LEGACY = os.environ.get("LEGACY", "") == "1"
SESSION_CMD = ("SMITHAY_USE_LEGACY=1 " if LEGACY else "") + "sh /bin/start-cosmic-leandros &"
os.makedirs(OUT, exist_ok=True)


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
    """Persistent QMP connection — reconnecting per event caps the rate far
    below the 60/s we need to reproduce the baseline measurement."""

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
        # One event per axis — the combined two-axis form is not what the
        # known-good M7z harness used, and a rejected command here looks
        # exactly like "the compositor ignored the pointer".
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
            except Exception as e:
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


# Order-independent `key=0xHEX` parser — same approach as
# notes/m9-fb-damage-clips/m9_analyze.py, so the two harnesses agree. A
# positional regex breaks silently the moment a new field is inserted
# mid-line: c5abb8d did exactly that, landing five dmg_* fields between
# flip_us and curs_up, which zeroed every field after flip_us for any
# parser (like the old STAT regex here) that matched by position. Keying
# off field NAMES instead survives any future insertion, wherever it lands.
KV = re.compile(r"([a-z_]+)=0x([0-9A-Fa-f]+)")


def parse_stats(text):
    out = []
    for line in text.splitlines():
        i = line.find("[DRMSTAT]")
        if i < 0:
            continue
        rec = {k: int(v, 16) for k, v in KV.findall(line[i:])}
        if "t" not in rec:            # 100 Hz ticks; every line has it
            continue
        out.append(rec)
    return out


def g(s, k):
    return s.get(k, 0)


def main():
    log(f"==== M8 cursor {ARCH} tag={TAG} drain={DRAIN} burst@{BURST_AT}s "
        f"for {BURST_LEN}s legacy={LEGACY}  {time.ctime()} ====")
    try:
        os.remove(SERIAL)
    except OSError:
        pass
    env = {"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}

    booted = False
    out = ""
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
        target=lambda: d("session", str(DRAIN), SESSION_CMD,
                         t=DRAIN + 40),
        daemon=True).start()
    log(f"[session launched; draining {DRAIN}s]")
    t0 = time.time()

    # Let the desktop settle, then screenshot before the burst.
    time.sleep(max(0, BURST_AT - 10 - (time.time() - t0)))
    q = Qmp()
    pre = f"{OUT}/m8-{ARCH}-{TAG}-pre.ppm"
    q.screendump(pre)
    log(f"[t={int(time.time()-t0)}] pre-burst screenshot -> {pre}")

    # ---- pointer burst: ~60 moves/s ----
    time.sleep(max(0, BURST_AT - (time.time() - t0)))
    log(f"[t={int(time.time()-t0)}] BURST START ({BURST_LEN}s @ ~60 moves/s)")
    burst_t0 = time.time()
    n = 0
    while time.time() - burst_t0 < BURST_LEN:
        # a slow lissajous so every move is a genuinely new position
        p = (time.time() - burst_t0)
        x = int(W * 0.5 + W * 0.35 * __import__("math").sin(p * 1.7))
        y = int(H * 0.5 + H * 0.30 * __import__("math").sin(p * 2.3))
        if q.move(x, y):
            n += 1
        time.sleep(1.0 / 60)
    burst_dur = time.time() - burst_t0
    log(f"[t={int(time.time()-t0)}] BURST END: {n} moves in {burst_dur:.1f}s "
        f"= {n/burst_dur:.1f} moves/s")

    post = f"{OUT}/m8-{ARCH}-{TAG}-post.ppm"
    q.screendump(post)
    time.sleep(4)

    # ---- report ----
    try:
        data = open(SERIAL, errors="replace").read()
    except OSError:
        log("no serial log"); clean(); return 1
    ct = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "",
                re.sub(r"\x1b[=>78]", "", data))
    open(f"{OUT}/m8-{ARCH}-{TAG}-serial.txt", "w").write(ct[-1200000:])

    stats = parse_stats(ct)
    log(f"\n---- {len(stats)} DRMSTAT samples ----")
    for s in stats:
        log(f"  t={g(s,'t')/100:7.2f}s flips_sub={g(s,'flips_sub'):5d} "
            f"flips_del={g(s,'flips_del'):5d} dirtyfb={g(s,'dirtyfb'):4d} "
            f"curs_up={g(s,'curs_up'):5d} curs_mv={g(s,'curs_mv'):6d} "
            f"atomic={g(s,'atomic'):6d} atest={g(s,'atest'):6d} "
            f"cplane={g(s,'cplane'):6d} flip_us={g(s,'flip_us')}")

    # Isolate the burst window by matching the guest tick range. The guest tick
    # and host clock share an origin only loosely, so use the LAST samples whose
    # pointer traffic is changing, falling back to the final two samples.
    #
    # Keyed on evpush, NOT curs_mv: curs_mv is identically 0 on the legacy KMS
    # path (no cursor plane exists there to move), so keying on it silently
    # picks a degenerate window and reports a plausible-looking-but-meaningless
    # flips/s for a legacy-path control. evpush is the guest-side evdev-event
    # counter (added 05bb0fe) and is nonzero on BOTH paths whenever pointer
    # motion actually reached the guest ring.
    exit_code = 0
    if len(stats) >= 2:
        best = None
        for a, b in zip(stats, stats[1:]):
            dt = (g(b, "t") - g(a, "t")) / 100.0
            if dt <= 0:
                continue
            dep = g(b, "evpush") - g(a, "evpush")
            if best is None or dep > best[0]:
                best = (dep, a, b, dt)
        if best is None:
            print("[m8_cursor] ERROR: no two DRMSTAT samples have a positive "
                  "tick delta — cannot pick a busiest window.",
                  file=sys.stderr)
            exit_code = 1
        else:
            dep, a, b, dt = best
            if dep <= 0:
                # evpush is flat across EVERY candidate window, not just the
                # chosen one: every window is equally degenerate, so there is
                # no meaningful "busiest" one to report. Printing a number
                # here would look like a real measurement (this is exactly
                # how a legacy-path control used to silently print a
                # degenerate 1.00 flips/s when the window was keyed on
                # curs_mv, which is identically 0 on legacy). Fail loudly
                # instead.
                print(f"[m8_cursor] ERROR: evpush never advanced across any "
                      f"of the {len(stats)} DRMSTAT samples — pointer motion "
                      f"never reached the guest ring. Every candidate window "
                      f"is degenerate; refusing to report a busiest-window "
                      f"flips/s figure.", file=sys.stderr)
                exit_code = 1
            else:
                dmv = g(b, "curs_mv") - g(a, "curs_mv")
                log(f"\n---- busiest window by evpush ({dt:.1f}s) ----")
                log(f"  evpush/s     : {dep/dt:.2f}")
                log(f"  page flips/s : {(g(b,'flips_sub')-g(a,'flips_sub'))/dt:.2f}   "
                    f"(BASELINE 0.9)")
                log(f"  delivered/s  : {(g(b,'flips_del')-g(a,'flips_del'))/dt:.2f}")
                log(f"  cursor mv/s  : {dmv/dt:.2f}")
                log(f"  cursor up/s  : {(g(b,'curs_up')-g(a,'curs_up'))/dt:.2f}")
                log(f"  atomic/s     : {(g(b,'atomic')-g(a,'atomic'))/dt:.2f}")
                log(f"  atomic TESTs : {g(b,'atest')} total")
                log(f"  cursor-plane mentions: {g(b,'cplane')} total "
                    f"(0 => compositor never tried the cursor plane)")
                log(f"  flip us/s    : {(g(b,'flip_us')-g(a,'flip_us'))/dt:.0f}")
    else:
        print(f"[m8_cursor] ERROR: only {len(stats)} DRMSTAT sample(s) "
              f"parsed — cannot pick a busiest window.", file=sys.stderr)
        exit_code = 1

    for pat in ("panic", "PANIC", "Fault", "Broken pipe", "Unknown id",
                "atomic", "ATOMIC", "cursor"):
        hits = [l for l in ct.splitlines() if pat in l]
        if hits:
            log(f"\n[grep '{pat}'] {len(hits)} lines, last 6:")
            for l in hits[-6:]:
                log("   " + l[:200])

    clean()
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
