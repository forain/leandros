#!/usr/bin/env python3
"""zinkbench — boot a --venus guest, start the COSMIC session with or without
Zink, drive a fixed input script for N seconds and report the [DRMSTAT]
counters, host-side QEMU thread CPU, and a VNC photograph of the GL scanout.

usage: zinkbench.py <tag> [--zink] [--dur 80] [--settle 40] [--log] [--keep]
"""
import os, sys, time, socket, select, json, re, subprocess, threading, struct

# Every path is env-driven so two trees can bench on one machine:
#   LEANDROS_RUN_ID   tags driver.py's sockets/logs (default zink)
#   LEANDROS_VNC_PORT the --venus VNC listener the photograph is taken from
#   ZINKBENCH_REPO    the tree whose driver.py and images to use (default: this one)
#   ZINKBENCH_OUT     where logs/PNGs go (default ~/zinkbench)
os.environ.setdefault("LEANDROS_RUN_ID", "zink")
REPO = os.environ.get("ZINKBENCH_REPO") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
os.chdir(REPO)
import driver  # noqa: E402

OUT = os.environ.get("ZINKBENCH_OUT") or os.path.expanduser("~/zinkbench")
os.makedirs(OUT, exist_ok=True)

args = sys.argv[1:]
TAG = args[0]
ZINK = "--zink" in args
KEEP = "--keep" in args
COMPLOG = "--log" in args
NOINPUT = "--noinput" in args
def opt(name, default):
    if name in args:
        return int(args[args.index(name) + 1])
    return default
DUR = opt("--dur", 80)
SETTLE = opt("--settle", 40)

LOGF = open(f"{OUT}/{TAG}-serial.log", "wb")
STAT = []          # (host_time, dict)
EVENTS = []        # (host_time, kind)
lock = threading.Lock()
stop = threading.Event()

def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)

STAT_RE = re.compile(rb"\[DRMSTAT\] (.*)")
KV_RE = re.compile(rb"(\w+)=0x([0-9a-fA-F]+)")

def serial_reader(s):
    buf = b""
    while not stop.is_set():
        r = select.select([s], [], [], 0.2)[0]
        if not r:
            continue
        try:
            chunk = s.recv(65536)
        except BlockingIOError:
            continue
        if not chunk:
            log("serial EOF")
            return
        LOGF.write(chunk); LOGF.flush()
        buf += chunk
        while b"\n" in buf:
            line, _, buf = buf.partition(b"\n")
            m = STAT_RE.search(line)
            if m:
                d = {k.decode(): int(v, 16) for k, v in KV_RE.findall(m.group(1))}
                with lock:
                    STAT.append((time.time(), d))
            if b"[FLIP]" in line or b"[SUBMIT]" in line or b"[STALL]" in line or b"[INP]" in line:
                with lock:
                    EVENTS.append((time.time(), line.decode(errors="replace")))

def send_line(s, line):
    payload = (line + "\n").encode()
    s.setblocking(True)
    for i in range(0, len(payload), 8):
        s.sendall(payload[i:i + 8]); time.sleep(0.02)
    s.setblocking(False)

# ---------------------------------------------------------------- VNC grab
def vnc_grab(port, path):
    try:
        so = socket.create_connection(("127.0.0.1", port), timeout=10)
        so.settimeout(30)
        def rd(n):
            b = b""
            while len(b) < n:
                c = so.recv(n - len(b))
                if not c: raise IOError("vnc eof")
                b += c
            return b
        ver = rd(12)
        so.sendall(b"RFB 003.008\n")
        n = rd(1)[0]
        types = rd(n)
        if 1 not in types: raise IOError(f"vnc sec types {types!r}")
        so.sendall(b"\x01")
        if struct.unpack(">I", rd(4))[0] != 0: raise IOError("vnc sec fail")
        so.sendall(b"\x01")
        w, h = struct.unpack(">HH", rd(4))
        pf = rd(16)
        bpp, depth, bigend, tc, rmax, gmax, bmax, rsh, gsh, bsh = struct.unpack(">BBBBHHHBBB", pf[:13])
        nl = struct.unpack(">I", rd(4))[0]; name = rd(nl)
        # SetEncodings: raw only
        so.sendall(struct.pack(">BxHi", 2, 1, 0))
        # FramebufferUpdateRequest non-incremental full
        so.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))
        from PIL import Image
        img = Image.new("RGB", (w, h))
        px = img.load()
        deadline = time.time() + 30
        got = 0
        while got < w * h and time.time() < deadline:
            mt = rd(1)[0]
            if mt != 0:
                # skip other messages crudely
                if mt == 2: continue
                if mt == 1:
                    rd(1); first, nc = struct.unpack(">HH", rd(4)); rd(6 * nc); continue
                if mt == 3:
                    rd(3); ln = struct.unpack(">I", rd(4))[0]; rd(ln); continue
                raise IOError(f"vnc msg {mt}")
            rd(1); nr = struct.unpack(">H", rd(2))[0]
            for _ in range(nr):
                x, y, rw, rh, enc = struct.unpack(">HHHHi", rd(12))
                if enc != 0: raise IOError(f"enc {enc}")
                data = rd(rw * rh * bpp // 8)
                bys = bpp // 8
                fmt = ">" if bigend else "<"
                for j in range(rh):
                    for i in range(rw):
                        off = (j * rw + i) * bys
                        v = int.from_bytes(data[off:off + bys], "big" if bigend else "little")
                        r = (v >> rsh) & rmax; g = (v >> gsh) & gmax; b = (v >> bsh) & bmax
                        px[x + i, y + j] = (r * 255 // rmax, g * 255 // gmax, b * 255 // bmax)
                got += rw * rh
        so.close()
        img.save(path)
        # crude summary: distinct colours + mean
        small = img.resize((64, 36))
        cols = set(small.getdata())
        log(f"VNC {w}x{h} name={name!r} saved {path} distinct64x36={len(cols)}")
        return True
    except Exception as e:
        log(f"VNC grab failed: {e}")
        return False

# ---------------------------------------------------------------- host CPU
def qemu_threads(pid):
    out = {}
    try:
        for t in os.listdir(f"/proc/{pid}/task"):
            try:
                comm = open(f"/proc/{pid}/task/{t}/comm").read().strip()
                st = open(f"/proc/{pid}/task/{t}/stat").read()
                f = st[st.rindex(")") + 2:].split()
                ut, stt = int(f[11]), int(f[12])
                out[t] = (comm, ut + stt)
            except Exception:
                pass
    except Exception:
        pass
    return out

def render_server_pids():
    try:
        return [int(p) for p in subprocess.run(["pgrep", "-f", "virgl_render_server"], capture_output=True, text=True).stdout.split()]
    except Exception:
        return []

def cpu_snapshot():
    snap = {}
    qp = driver._qemu_pid()
    if qp: snap[("qemu", qp)] = qemu_threads(qp)
    for rp in render_server_pids():
        snap[("vrs", rp)] = qemu_threads(rp)
    return snap

def cpu_diff(a, b, secs):
    lines = []
    for key in b:
        if key not in a: continue
        for t, (comm, cb) in b[key].items():
            ca = a[key].get(t, (comm, 0))[1]
            d = (cb - ca) / os.sysconf("SC_CLK_TCK")
            if d > 0.05:
                lines.append((d, f"{key[0]}:{comm}[{t}]"))
    lines.sort(reverse=True)
    return "\n".join(f"   {d/secs*100:6.1f}% {n}" for d, n in lines)

# ---------------------------------------------------------------- input
def input_loop(dur):
    import math
    ses = driver._qmp_open()
    t0 = time.time()
    i = 0
    try:
        while time.time() - t0 < dur and not stop.is_set():
            ang = i * 0.2
            x = int(16000 + 8000 * math.cos(ang)); y = int(16000 + 8000 * math.sin(ang))
            ev = [{"type": "abs", "data": {"axis": "x", "value": x}},
                  {"type": "abs", "data": {"axis": "y", "value": y}}]
            driver._qmp_command(ses, "input-send-event", {"events": ev}, timeout=5)
            with lock: EVENTS.append((time.time(), "host:move"))
            if i % 50 == 25:   # every 5 s
                for down in (True, False):
                    driver._qmp_command(ses, "input-send-event", {"events": [
                        {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": "shift"}}}]}, timeout=5)
                    with lock: EVENTS.append((time.time(), f"host:key{'down' if down else 'up'}"))
                    time.sleep(0.05)
            i += 1
            time.sleep(max(0, (t0 + i * 0.1) - time.time()))
    finally:
        ses.close()

# ---------------------------------------------------------------- main
def main():
    log(f"=== zinkbench {TAG} zink={ZINK} dur={DUR} settle={SETTLE} ===")
    driver.cmd_stop() if driver._qemu_pid() else None
    subprocess.run(["pkill", "-9", "-f", f"leandros-{os.environ['LEANDROS_RUN_ID']}-serial"], capture_output=True)
    time.sleep(1)
    driver.cmd_start("x86_64", "uefi", venus=True)
    driver.cmd_login("root", "root")
    # The default boot is a graphical login (greetd -> cosmic-comp, softpipe),
    # which owns the display; the census needs the bare text login so the
    # session it launches is the only compositor. The marker is persistent on
    # the root image, so it costs one reboot per freshly built image.
    probe = driver._serial_send(
        "test -e /etc/leandros/text-login && echo HAVE_MARKER || "
        "(mkdir -p /etc/leandros; echo 1 > /etc/leandros/text-login; echo SET_MARKER)", timeout=6)
    if "SET_MARKER" in probe or "HAVE_MARKER" not in probe:
        log("text-login marker set; rebooting so no greeter owns the display")
        driver.cmd_stop(); time.sleep(2)
        driver.cmd_start("x86_64", "uefi", venus=True)
        driver.cmd_login("root", "root")
    s = driver._connect_with_retry(driver.SERIAL_SOCK)
    s.setblocking(False)
    th = threading.Thread(target=serial_reader, args=(s,), daemon=True); th.start()
    redir = "" if COMPLOG else " >/dev/null 2>&1"
    pre = "LEANDROS_ZINK=1 " if ZINK else ""
    if "--env" in args:
        pre = args[args.index("--env") + 1] + " " + pre
    cmdline = f"{pre}sh /bin/start-cosmic-leandros{redir} &"
    log("launch:", cmdline)
    send_line(s, cmdline)
    t_launch = time.time()
    time.sleep(SETTLE)
    with lock: n0 = len(STAT); s0 = STAT[-1][1] if STAT else {}
    log(f"after settle: {n0} DRMSTAT samples, flips_sub={s0.get('flips_sub')} ctrlq_n={s0.get('ctrlq_n')}")
    vnc_grab(driver.VENUS_VNC_PORT, f"{OUT}/{TAG}-settled.png")
    cpu0 = cpu_snapshot()
    tw0 = time.time()
    with lock: EVENTS.append((tw0, "host:window-start"))
    if not NOINPUT:
        it = threading.Thread(target=input_loop, args=(DUR,), daemon=True); it.start()
        it.join(DUR + 30)
    else:
        time.sleep(DUR)
    tw1 = time.time()
    with lock: EVENTS.append((tw1, "host:window-end"))
    cpu1 = cpu_snapshot()
    time.sleep(3)
    vnc_grab(driver.VENUS_VNC_PORT, f"{OUT}/{TAG}-end.png")
    # ---- analysis
    with lock:
        samples = [(t, d) for (t, d) in STAT if tw0 - 2.5 <= t <= tw1 + 0.5]
        evs = list(EVENTS)
    if len(samples) < 2:
        log("NOT ENOUGH DRMSTAT SAMPLES", len(samples))
    else:
        a, b = samples[0][1], samples[-1][1]
        secs = samples[-1][0] - samples[0][0]
        def dl(k): return b.get(k, 0) - a.get(k, 0)
        fl = dl("flips_sub"); cq = dl("ctrlq_n"); cqus = dl("ctrlq_us")
        log(f"WINDOW {secs:.1f}s: flips_sub Δ={fl} => {fl/secs:.3f} fps; flips_del Δ={dl('flips_del')}; "
            f"atomic Δ={dl('atomic')} curs_mv Δ={dl('curs_mv')} evpush Δ={dl('evpush')} pollwake Δ={dl('pollwake')}")
        log(f"  ctrlq_n Δ={cq} ctrlq_us Δ={cqus} ({cqus/secs/1e4:.2f}% of wall) mean={cqus/max(cq,1):.0f}us "
            f"max_sofar={b.get('ctrlq_max')} timeouts={b.get('ctrlq_to')} flip_us Δ={dl('flip_us')} ({dl('flip_us')/secs/1e4:.2f}%)")
        # per-sample timeline of flips
        tl = []
        for (t1, d1), (t2, d2) in zip(samples, samples[1:]):
            tl.append((round(t2 - tw0, 1), d2.get("flips_sub", 0) - d1.get("flips_sub", 0),
                       d2.get("ctrlq_n", 0) - d1.get("ctrlq_n", 0),
                       d2.get("ctrlq_us", 0) - d1.get("ctrlq_us", 0),
                       d2.get("evpush", 0) - d1.get("evpush", 0)))
        log("  timeline (t, Δflips, Δctrlq_n, Δctrlq_us, Δevpush):")
        for row in tl: print("    ", row)
        zero = sum(1 for r in tl if r[1] == 0)
        log(f"  {zero}/{len(tl)} 2s-samples with zero flips")
    log("HOST CPU over window:\n" + cpu_diff(cpu0, cpu1, tw1 - tw0))
    with open(f"{OUT}/{TAG}-events.json", "w") as f:
        json.dump({"window": [tw0, tw1], "events": evs, "stat": samples if len(samples) >= 2 else []}, f)
    stop.set()
    if not KEEP:
        driver.cmd_stop()
    log("done")

if __name__ == "__main__":
    main()
