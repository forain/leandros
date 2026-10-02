#!/usr/bin/env python3
"""coldstart-bench.py: first-launch vs second-launch latency of COSMIC apps and
the portal file chooser, from a fresh boot, on any arch/accelerator.

One boot per invocation: driver.py start --virgl (CS_GPU), serial root login,
greeter login as leandro, then the steps in CS_STEPS, each timed from the
triggering key/click/command to the first screen change (VNC grab polled
every ~0.3 s, so +-0.3 s) and to a settled frame:
  term / settings      launcher (Super, type, Enter); window closed with Super+Q
  wallpaper            cosmic-settings Wallpaper page, maximized, "Add image"
                       (the portal FileChooser dialog through cosmic-settings)
  portal               leandros-sysbus portal open-file from the root serial
                       shell (no parent window), cancelled with Escape
A suffix makes a repeat (term2, wallpaper2 ...). After each step a Ctrl-T dump
is taken; with sched::pcsample::ENABLED it carries per-process user/kernel/idle
samples for the window, summarised as "pcs". Host CPU of the QEMU process is
sampled at 1 Hz (host-cpu.log).

env: CS_REPO (worktree) CS_OUT (output dir) CS_ARCH [CS_GPU=virgl]
     [CS_STEPS] [CS_SETTLE=45] [CS_KEEP=1 leaves QEMU running]
     LEANDROS_RUN_ID (default coldstart) LEANDROS_VNC_PORT
out: results.json, events.log, serial.log (host-timestamped, nothing dropped),
     host-cpu.log, dump-<step>.txt, <step>-shown.png
"""
import os, sys, time, select, threading, subprocess, re, json, platform, collections

REPO = os.environ["CS_REPO"]
OUT = os.environ["CS_OUT"]
os.environ.setdefault("LEANDROS_RUN_ID", "coldstart")
os.environ.setdefault("LEANDROS_NO_VIEWER", "1")
os.environ.setdefault("LEANDROS_QEMU_MEM", "4G")
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
os.chdir(REPO)
import driver  # noqa: E402

os.makedirs(OUT, exist_ok=True)
DRV = f"{REPO}/.claude/skills/run-leandros/driver.py"
T0 = time.time()
LOGF = open(f"{OUT}/events.log", "a")


def log(*a):
    line = f"{time.time()-T0:8.2f} " + " ".join(str(x) for x in a)
    print(line, flush=True)
    LOGF.write(line + "\n"); LOGF.flush()


def drv(*a, timeout=600):
    r = subprocess.run([sys.executable, DRV, *a], capture_output=True, text=True, timeout=timeout)
    return r.stdout + r.stderr


# ---------------------------------------------------------------- serial
class Serial:
    def __init__(self):
        self.s = driver._connect_with_retry(driver.SERIAL_SOCK)
        self.buf = bytearray()
        self.lock = threading.Lock()
        self.f = open(f"{OUT}/serial.log", "ab")
        self.stop = False
        threading.Thread(target=self._rd, daemon=True).start()

    def _rd(self):
        pending = b""
        while not self.stop:
            try:
                r = select.select([self.s], [], [], 0.2)[0]
                if not r:
                    continue
                c = self.s.recv(65536)
            except Exception:
                break
            if not c:
                break
            if b"\x1b[6n" in c:
                try: self.s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                except Exception: pass
            with self.lock:
                self.buf += c
            pending += c
            while b"\n" in pending:
                ln, pending = pending.split(b"\n", 1)
                self.f.write(b"%.2f " % (time.time() - T0) + ln + b"\n")
            self.f.flush()

    def mark(self):
        with self.lock:
            return len(self.buf)

    def since(self, m):
        with self.lock:
            return bytes(self.buf[m:])

    def send(self, data: bytes):
        self.s.sendall(data)

    def cmd(self, c, timeout=20, wait=None):
        """Type a shell line (8-byte chunks, the reedline-safe pace) and wait
        for a regex `wait` (default: the next root prompt)."""
        self.send(b"\r"); time.sleep(0.4)
        m = self.mark()
        data = c.encode() + b"\r"
        for i in range(0, len(data), 8):
            self.send(data[i:i+8]); time.sleep(0.02)
        pat = re.compile((wait or r"brush-[0-9.]+# [\x1b78]*(\r?\n|$)").encode())
        dl = time.time() + timeout
        while time.time() < dl:
            out = driver._strip_ansi(self.since(m))
            if wait:
                if pat.search(out):
                    return out.decode(errors="replace")
            else:
                tail = c.encode()[-12:]
                i = out.rfind(tail)
                if i >= 0 and re.search(rb"brush-[0-9.]+# ", out[i + len(tail):]):
                    return out.decode(errors="replace")
            time.sleep(0.2)
        return driver._strip_ansi(self.since(m)).decode(errors="replace") + "\n<TIMEOUT>"

    def ctrl_t(self, settle=12):
        m = self.mark()
        self.send(b"\x14")
        dl = time.time() + 60
        # wait for the PCS drain to end (if sampling is compiled in)
        while time.time() < dl:
            s = self.since(m)
            if b"[PCS] end" in s:
                break
            if time.time() > dl - 60 + settle and b"[PCS]" not in s:
                break
            time.sleep(0.5)
        return self.since(m).decode(errors="replace")


# ---------------------------------------------------------------- screen
def grab():
    p = f"{OUT}/.g.ppm"
    port = int(open(driver.VNC_STATE_FILE).read().strip())
    driver._vnc_grab_ppm(port, p)
    data = open(p, "rb").read()
    # header P6\nW H\n255\n
    parts = data.split(b"\n", 3)
    return data[len(parts[0]) + len(parts[1]) + len(parts[2]) + 3:]


def save(name):
    p = f"{OUT}/{name}.ppm"
    port = int(open(driver.VNC_STATE_FILE).read().strip())
    driver._vnc_grab_ppm(port, p)
    try:
        subprocess.run(["sips", "-s", "format", "png", p, "--out", p[:-4] + ".png"],
                       capture_output=True)
        os.unlink(p)
    except FileNotFoundError:
        pass  # no sips (Linux host): keep the PPM


def diff(a, b, stride=97):
    n = min(len(a), len(b))
    if n == 0:
        return 1.0
    d = sum(1 for i in range(0, n, stride) if abs(a[i] - b[i]) > 24)
    return d / (n // stride)


def wait_change(base, t0, thresh=0.04, timeout=150, stable_needed=2):
    """Poll the screen until it differs from `base` by > thresh (first frame),
    then until two consecutive grabs agree (settled). Returns (t_first, t_settled)."""
    t_first = None; prev = None; stable = 0
    dl = t0 + timeout
    while time.time() < dl:
        cur = grab()
        now = time.time() - t0
        if t_first is None:
            if diff(cur, base) > thresh:
                t_first = now; prev = cur
        else:
            if diff(cur, prev, 211) < 0.003:
                stable += 1
                if stable >= stable_needed:
                    return t_first, now
            else:
                stable = 0
            prev = cur
        time.sleep(0.25)
    return t_first, None


# ---------------------------------------------------------------- keys
M = {" ": "spc", "-": "minus", "/": "slash", ".": "dot"}


def key(k, d=0.25):
    driver._monitor_send(f"sendkey {k}"); time.sleep(d)


def typ(s):
    for c in s:
        key(M.get(c, c), 0.15)


# ---------------------------------------------------------------- host CPU
def _cputime_s(pid):
    if os.path.exists(f"/proc/{pid}/stat"):
        try:
            f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
            return (int(f[11]) + int(f[12])) / os.sysconf("SC_CLK_TCK")
        except Exception:
            return None
    try:
        out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    except Exception:
        return None
    if not out:
        return None
    # [[dd-]hh:]mm:ss.cc
    d = 0
    if "-" in out:
        dd, out = out.split("-", 1); d = int(dd) * 86400
    parts = [float(x) for x in out.split(":")]
    s = 0.0
    for p in parts:
        s = s * 60 + p
    return d + s


def _mtl_pids():
    out = subprocess.run(["pgrep", "-f", "MTLCompilerService.xpc"], capture_output=True, text=True).stdout
    return [int(x) for x in out.split()]


class HostSampler:
    """1 Hz: QEMU process CPU, and (Mac) the summed CPU of every MTLCompilerService
    process (host-wide, not only QEMU's: Metal shader compiles show up here)."""
    def __init__(self, qpid):
        self.qpid = qpid; self.rows = []; self.stop = False
        self.f = open(f"{OUT}/host-cpu.log", "a")
        threading.Thread(target=self._run, daemon=True).start()

    def _run(self):
        lastq = _cputime_s(self.qpid); lastm = None
        while not self.stop:
            time.sleep(1.0)
            q = _cputime_s(self.qpid)
            m = sum((_cputime_s(p) or 0) for p in _mtl_pids()) if platform.system() == "Darwin" else 0
            dq = (q - lastq) if (q is not None and lastq is not None) else -1
            dm = (m - lastm) if lastm is not None else 0
            lastq, lastm = q, m
            row = (round(time.time() - T0, 1), round(dq * 100), round(dm * 100))
            self.rows.append(row)
            self.f.write("%.1f qemu=%d%% mtl=%d%%\n" % row); self.f.flush()

    def window(self, a, b):
        r = [x for x in self.rows if a <= x[0] <= b]
        if not r:
            return {}
        return dict(qemu_avg=round(sum(x[1] for x in r) / len(r)), mtl_cpu_s=round(sum(x[2] for x in r) / 100, 1))


# ---------------------------------------------------------------- steps

ARCH = os.environ.get("CS_ARCH", "aarch64")
GPU = os.environ.get("CS_GPU", "virgl")
STEPS = os.environ.get("CS_STEPS", "term term2 settings settings2 wallpaper wallpaper2 portal portal2").split()
SETTLE = float(os.environ.get("CS_SETTLE", "45"))
res = collections.OrderedDict()


def dump(name):
    out = SER.ctrl_t()
    open(f"{OUT}/dump-{name}.txt", "w").write(out)
    # PCS summary per tgid
    names = {}
    for m in re.finditer(r"\[TASKS\] pid=(\d+) tgid=(\d+)[^\n]*?\((/[^)\s]+)\)", out):
        names[int(m.group(2), 10)] = m.group(3).rsplit("/", 1)[-1]
    cnt = collections.Counter(); tot = 0
    for m in re.finditer(r"\[PCS\] ([0-9a-f]+) ([0-9a-f]+) ([0-9a-f]+) ([0-9a-f]+) ([uki]) ([0-9a-f]+)", out):
        tg = int(m.group(2), 16); mode = m.group(5); tot += 1
        cnt[(names.get(tg, str(tg)) if tg else "idle", mode)] += 1
    top = [(f"{k[0]}:{k[1]}", v) for k, v in cnt.most_common(14)]
    return dict(samples=tot, top=top)


def settle_quiet(maxwait=60):
    """Wait until two grabs 3 s apart agree."""
    prev = grab(); t = time.time()
    while time.time() - t < maxwait:
        time.sleep(3); cur = grab()
        if diff(cur, prev, 211) < 0.002:
            return round(time.time() - t, 1)
        prev = cur
    return None


def close_window():
    key("meta_l-q", 0.5); time.sleep(3)


def click(px, py, W=1280, H=800):
    driver.qmp_pointer_abs(int(px*32767/W), int(py*32767/H)); time.sleep(0.3)
    so = driver._qmp_open()
    for down in (True, False):
        driver._qmp_command(so, "input-send-event", {"events": [{"type": "btn", "data": {"down": down, "button": "left"}}]})
        time.sleep(0.12)
    so.close()


def wallpaper(step):
    """cosmic-settings Wallpaper page -> maximize -> 'Add image' -> time the dialog."""
    r = launch(step + "-settings", "wallpaper", close=False)
    key("meta_l-m", 0.5); settle_quiet(30)
    save(step + "-page")
    base = grab(); t1 = time.time()
    # "Add image" on the maximized Wallpaper page (measured per framebuffer size)
    if len(base) == 1920 * 1080 * 3:
        click(1444, 602, 1920, 1080)
    else:
        click(1135, 603)
    f, s = wait_change(base, t1, thresh=0.04, timeout=200)
    log(step, "dialog first=", f, "settled=", s)
    save(f"{step}-shown")
    d = dict(first=f, settled=s, host=HS.window(t1 - T0, t1 - T0 + (s or f or 200)))
    d["pcs"] = dump(step)
    log(step, json.dumps(d))
    key("esc", 1.0); time.sleep(3); close_window()
    return dict(settings=r, dialog=d)


def launch(step, query, close=True):
    settle_quiet(20)
    base = grab()
    t0 = time.time(); key("meta_l", 0)
    lf, ls = wait_change(base, t0, thresh=0.02, timeout=60)
    log(step, "launcher first=", lf, "settled=", ls)
    typ(query); time.sleep(2.5)
    t1 = time.time(); key("ret", 0)
    # phase 1: launcher disappears (screen returns near base), max 5 s
    dl = time.time() + 5
    while time.time() < dl:
        if diff(grab(), base) < 0.02:
            break
        time.sleep(0.1)
    f, s = wait_change(base, t1, thresh=0.04, timeout=180)
    log(step, "window first=", f, "settled=", s)
    save(f"{step}-shown")
    r = dict(launcher_first=lf, launcher_settled=ls, first=f, settled=s,
             host=HS.window(t1 - T0, t1 - T0 + (s or f or 180)))
    r["pcs"] = dump(step)
    log(step, json.dumps(r))
    if close:
        close_window()
    return r


BUS = None


def portal(step):
    global BUS
    settle_quiet(20)
    if BUS is None:
        o = SER.cmd("ls /run/user/1000", 15)
        log("rundir:", o.replace("\n", " | ")[:300])
        BUS = "unix:path=/run/user/1000/bus"
    base = grab()
    t1 = time.time()
    SER.cmd(f"DBUS_SESSION_BUS_ADDRESS={BUS} /usr/libexec/leandros-sysbus portal open-file 300 > /tmp/{step}.out 2>&1 &", 10)
    f, s = wait_change(base, t1, thresh=0.04, timeout=200)
    log(step, "dialog first=", f, "settled=", s)
    save(f"{step}-shown")
    r = dict(first=f, settled=s, host=HS.window(t1 - T0, t1 - T0 + (s or f or 200)))
    r["pcs"] = dump(step)
    log(step, json.dumps(r))
    key("esc", 1.0); time.sleep(3)
    r["probe"] = SER.cmd(f"cat /tmp/{step}.out", 10)[-400:]
    return r


def main():
    r = drv("start", ARCH, f"--{GPU}")
    log("start", r[-300:].replace("\n", " | "))
    if "already" in r.lower():
        sys.exit("QEMU already running for this run id")
    qpid = driver._qemu_pid()
    open(f"{OUT}/qemu.pid", "w").write(str(qpid))
    global HS, SER
    HS = HostSampler(qpid)
    log("qemu pid", qpid)
    log("login", drv("login", "root", "root", timeout=240)[-120:].replace("\n", " | "))
    SER = Serial()
    tb = time.time()
    # greeter: wait for it to settle
    time.sleep(20)
    q = settle_quiet(120); log("greeter settled after", q)
    save("1-greeter")
    base = grab()
    typ("leandro"); tl = time.time(); key("ret", 0)
    f, s = wait_change(base, tl, thresh=0.10, timeout=240, stable_needed=4)
    log("session first=", f, "settled=", s)
    res["login"] = dict(first=f, settled=s)
    time.sleep(SETTLE)
    save("2-session")
    res["login"]["pcs"] = dump("login")
    for st in STEPS:
        try:
            if st.startswith("term"):
                res[st] = launch(st, "terminal")
            elif st.startswith("settings"):
                res[st] = launch(st, "settings")
            elif st.startswith("wallpaper"):
                res[st] = wallpaper(st)
            elif st.startswith("portal"):
                res[st] = portal(st)
        except Exception as e:
            log(st, "ERROR", repr(e)); res[st] = dict(error=repr(e))
        json.dump(res, open(f"{OUT}/results.json", "w"), indent=1)
    s = open(f"{OUT}/serial.log", errors="replace").read()
    res["counts"] = dict(pf=len(re.findall(r"\[PF\]", s)), segv=s.count("SEGV"),
                         panic=len(re.findall(r"(?i)panic", s)), wdog=s.count("[WDOG]"))
    json.dump(res, open(f"{OUT}/results.json", "w"), indent=1)
    if os.environ.get("CS_KEEP") != "1":
        SER.stop = True
        log("stop", drv("stop")[-100:])
    log("DONE")


if __name__ == "__main__":
    main()
