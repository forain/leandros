#!/usr/bin/env python3
"""M13 — stage /usr/share/cosmic/ and measure whether COSMIC keybindings fire.

    python3 artifacts/m13_cosmic_config.py <arch> <before|after> [outdir]

WHAT IS BEING MEASURED

cosmic-config resolves a component's system defaults as

    system_path = xdg::BaseDirectories::with_prefix("cosmic")
                      .find_data_file("<name>/v<version>")

(libcosmic cosmic-config/src/lib.rs:203,236) — it looks for the DIRECTORY
/usr/share/cosmic/<name>/v<N>/ on XDG_DATA_DIRS, then reads each key as a bare
extensionless file holding a RON value (:481-487).  When the directory is
absent, `system_path` is None and every key lookup returns NoConfigDirectory.

Two silent swallows follow from that:
  * `defaults` (cosmic-comp/data/keybindings.ron) IS the whole keybinding
    table.  shortcuts::shortcuts() falls back to Shortcuts::default() — an
    EMPTY HashMap — so with the file absent cosmic-comp has no bindings at all.
  * `system_actions` maps Action::System(..) to a command; actions.rs:1016 is
    `if let Some(command) = ...get(&system)`, so an empty map is a no-op with
    no log line.

THE EXPERIMENT — three bindings, so a negative result stays interpretable:

  Super+F9     user `custom` binding  + user `system_actions`  -> touch a file
               CONTROL. Independent of /usr/share/cosmic entirely. If this
               fires, the whole host->virtio-keyboard->evdev->compositor->
               spawn chain works, and any failure below is config, not input.
               F9 is unbound in upstream keybindings.ron, so it cannot be
               confused with a system default.

  Super+b      SYSTEM `defaults` binding + user `system_actions` -> touch
               TREATMENT-WITNESS. Reaching the touch requires the staged
               `defaults` file (the binding) but not the staged
               `system_actions` (the command is overridden locally). A
               filesystem witness, so it needs nothing to render.

  Super+slash  SYSTEM `defaults` binding + SYSTEM `system_actions`
               TREATMENT-VISUAL. Pure upstream, end to end, spawns
               cosmic-launcher. Judged on pixels.

  Super+a      same, System(AppLibrary) -> cosmic-app-library.

CHANNEL DISCIPLINE (each of these burned a previous lane)
  * Serial, QMP and HMP are separate single-client sockets.  driver.py opens
    and closes serial per subcommand, so `start`/`login` must FINISH before
    the persistent Serial here connects.  After that, driver.py is never
    called again — screendumps go over the QMP socket we already hold.
  * Exactly ONE Qmp object per run.  A second QMP client blocks forever on the
    greeting, `self.f` goes None, and every _send() then returns False without
    raising: zero events injected, no traceback.  We abort instead (see the
    `q.f is None` check) and assert q.sent/q.rejected at the end.
  * The session's stdout/stderr is redirected to a guest file rather than left
    on the console.  Commands sent ~180 s into an unredirected session do not
    execute — the console saturates.
"""
import hashlib
import json
import os
import re
import select
import socket
import subprocess
import sys
import time

REPO = os.path.expanduser("~/code/leandros")
DRIVER = f"{REPO}/.claude/skills/run-leandros/driver.py"
SERIAL_SOCK = "/tmp/leandros-serial.sock"
QMP_SOCK = "/tmp/leandros-qmp.sock"

ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
PHASE = sys.argv[2] if len(sys.argv) > 2 else "before"
OUT = sys.argv[3] if len(sys.argv) > 3 else f"/tmp/m13/{PHASE}-{ARCH}"
os.makedirs(OUT, exist_ok=True)

LOGF = open(f"{OUT}/m13-{PHASE}-{ARCH}.log", "w", buffering=1)
SHORTCUT_DIR = "/root/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1"
GUEST_LOG = "/tmp/cosmic.log"

# x86_64 has no hardware virtualisation on an Apple Silicon host: it runs under
# TCG and everything -- boot, session start, each software recomposite -- is
# several times slower than aarch64 under HVF. Budget for that rather than
# reading a slow session as a dead one.
TCG = (ARCH != "aarch64")
WAIT_LOOPS = 300 if TCG else 90      # x2 s => 600 s vs 180 s
SETTLE = 90 if TCG else 30
WINDOW = 45 if TCG else 20           # per keybinding witness window
QUIET = 90 if TCG else 45            # console-silent window for the idle series
LONGWIN = 90 if TCG else 45          # per screendump-series window


def log(msg=""):
    print(msg, flush=True)
    LOGF.write(msg + "\n")


def d(*a, t=300, env=None):
    e = dict(os.environ)
    e.update(env or {})
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True,
                           text=True, timeout=t, env=e)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


# ---------------------------------------------------------------- serial ----
class Serial:
    """Held open for the whole run; QEMU's serial chardev serves one client."""

    def __init__(self, tee=None):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SERIAL_SOCK)
        self.s.setblocking(False)
        self.tee = open(tee, "ab", buffering=0) if tee else None
        self.buf = b""
        self.drain(0.5)

    def _stash(self, chunk):
        self.buf += chunk
        if self.tee:
            self.tee.write(chunk)

    def drain(self, secs):
        end = time.time() + secs
        while time.time() < end:
            if select.select([self.s], [], [], 0.1)[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return
                self._stash(c)

    # 8 bytes / 20 ms is fine at an idle prompt but DROPS CHARACTERS once the
    # COSMIC session is running: a measured run turned `grep -c "failed` into
    # `grepto` and `/tmp/cosmic.log` into `/t/cosmic.log`. Worse, a mangled
    # quote left brush in `> ` continuation and every later command was
    # swallowed as more of that string. Slow the pacing down once the session
    # is up, and keep post-session commands short.
    chunk = 8
    gap = 0.02

    def send(self, cmd):
        # Drop what is buffered first: a control satisfiable by text that
        # predates the command is not a control.
        self.buf = b""
        payload = (cmd + "\n").encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), self.chunk):
            self.s.sendall(payload[i:i + self.chunk])
            time.sleep(self.gap)
        self.s.setblocking(False)

    def sync(self):
        """Return the line editor to a known state.

        A lone newline cannot escape quote-continuation; ^C can. Without this,
        one dropped quote character poisons every subsequent command.
        """
        self.s.setblocking(True)
        self.s.sendall(b"\x03")
        self.s.setblocking(False)
        self.pump(0.35)
        self.s.setblocking(True)
        self.s.sendall(b"\n")
        self.s.setblocking(False)
        self.pump(0.35)
        self.buf = b""

    def pump(self, secs):
        end = time.time() + secs
        while True:
            left = end - time.time()
            if left <= 0:
                return
            if select.select([self.s], [], [], min(0.2, left))[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return
                if b"\x1b[6n" in c:              # answer CPR or reedline hangs
                    self.s.setblocking(True)
                    self.s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                    self.s.setblocking(False)
                self._stash(c)

    def read_until(self, pattern, timeout):
        end = time.time() + timeout
        while True:
            txt = self.buf.decode("utf-8", "replace")
            m = pattern.search(txt)
            if m:
                self.buf = txt[m.end():].encode("utf-8", "replace")
                return m, txt[:m.end()]
            if time.time() >= end:
                return None, txt
            self.pump(0.4)


DONE = re.compile(r"M13-(\d+)-(\d+)-DONE")
SEQ = [0]
ANSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[()][B0]")


def clean(raw):
    """Render the serial transcript the way a terminal would.

    brush redraws the whole input line after every 8-byte write, each redraw
    prefixed by a CR. Collapsing CRs away (rather than honouring them) glues
    every redraw into one ~1 kB pseudo-line that then swallows the real output
    under any display limit — and hides it from line-anchored regexes.
    """
    out = []
    for line in ANSI.sub("", raw).split("\n"):
        out.append(line.split("\r")[-1])
    return "\n".join(out)


def interesting(body):
    """Drop the command echo and bare prompts, keep real output."""
    keep = []
    for line in body.split("\n"):
        s = line.strip()
        if not s or "M13-$?-" in s or s.startswith("brush-") and s.endswith("#"):
            continue
        if re.fullmatch(r"M13-\d+-\d+-DONE", s):
            continue
        keep.append(s)
    return "\n".join(keep)


def run(ser, cmd, secs=15):
    """Run a guest command and return (rc, output).

    The marker is printed as `M13-$?-DONE`; the terminal echo of the typed
    command therefore contains a literal `$?`, which cannot match `(\\d+)` —
    so the regex can only be satisfied by the shell's own output, never by
    the echo of the command that asks for it.

    A bare newline is sent first.  The console read path also drains evdev
    (syscall.rs:3962 `console_input_pending() || evdev_server::has_key_event`),
    so a key we inject over QMP can end up as a stray character on the shell's
    current line; without the sync it would silently prefix the next command.
    The stray line executes harmlessly on its own and we start clean.
    """
    # The sequence number makes each marker unique, so a marker still in flight
    # from the previous command can never satisfy this one's wait and truncate
    # its output. The `$?` is unexpanded in the echo of the typed line, so only
    # the shell's own output can match. One retry, because a dropped character
    # on a busy console is a transport fault, not a result.
    body = ""
    for attempt in (1, 2):
        SEQ[0] += 1
        seq = SEQ[0]
        if attempt == 1:
            ser.send("")
            ser.pump(0.4)
        else:
            ser.sync()
        ser.send(f"{cmd}; echo M13-$?-{seq}-DONE")
        pat = re.compile(r"M13-(\d+)-%d-DONE" % seq)
        m, txt = ser.read_until(pat, secs)
        body = clean(txt)
        if m is not None:
            return int(m.group(1)), body
        log(f"  [run] no marker for {cmd[:60]!r} (attempt {attempt})")
    return None, body


# ------------------------------------------------------------------- qmp ----
class Qmp:
    def __init__(self, w=1920, h=1080):
        self.f = None
        self.w, self.h = w, h
        self.sent = 0
        self.rejected = 0
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(10)
            s.connect(QMP_SOCK)
            self.s = s
            self.f = s.makefile("rwb")
            self.f.readline()
            self.f.write(b'{"execute":"qmp_capabilities"}\n')
            self.f.flush()
            self.f.readline()
        except Exception as e:
            log(f"[qmp] connect failed: {e}")
            self.f = None

    def _cmd(self, obj):
        self.f.write((json.dumps(obj) + "\n").encode())
        self.f.flush()
        # Skip asynchronous events; only a return/error answers the command.
        while True:
            line = self.f.readline().decode(errors="replace")
            if not line:
                return ""
            if '"event"' in line and '"return"' not in line:
                continue
            return line

    def _send(self, events):
        if not self.f:
            return False
        try:
            resp = self._cmd({"execute": "input-send-event",
                              "arguments": {"events": events}})
            self.sent += 1
            if "return" not in resp:
                self.rejected += 1
                if self.rejected <= 3:
                    log(f"[qmp] REJECTED {json.dumps(events)[:120]} -> "
                        f"{resp.strip()[:200]}")
                return False
            return True
        except Exception as e:
            log(f"[qmp] send failed: {e}")
            self.f = None
            return False

    def key(self, qcode, down):
        return self._send([{"type": "key", "data": {
            "key": {"type": "qcode", "data": qcode}, "down": bool(down)}}])

    def tap(self, qcode, mods=()):
        for m in mods:
            self.key(m, True)
            time.sleep(0.05)
        self.key(qcode, True)
        time.sleep(0.08)
        self.key(qcode, False)
        for m in reversed(mods):
            time.sleep(0.05)
            self.key(m, False)

    def move(self, x, y):
        # One event per axis: the combined two-axis form is not what was
        # measured to work here, and a rejected command is indistinguishable
        # from "the compositor ignored the pointer".
        ok = True
        for axis, val, span in (("x", x, self.w), ("y", y, self.h)):
            v = max(0, min(span - 1, int(val)))
            ok &= self._send([{"type": "abs", "data": {
                "axis": axis, "value": int(v * 0x7FFF / span)}}])
        return ok

    def screendump(self, path):
        if not self.f:
            return False
        try:
            resp = self._cmd({"execute": "screendump",
                              "arguments": {"filename": path}})
            return "return" in resp
        except Exception as e:
            log(f"[qmp] screendump failed: {e}")
            return False


# ------------------------------------------------------------------- ppm ----
def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    if not data.startswith(b"P6"):
        return None
    idx, vals = 2, []
    while len(vals) < 3:
        while idx < len(data) and data[idx:idx + 1].isspace():
            idx += 1
        if data[idx:idx + 1] == b"#":
            while idx < len(data) and data[idx:idx + 1] != b"\n":
                idx += 1
            continue
        s = idx
        while idx < len(data) and not data[idx:idx + 1].isspace():
            idx += 1
        vals.append(int(data[s:idx]))
    w, h, _ = vals
    return w, h, data[idx + 1:]


GEOM = [None]


def census(path, label):
    md5 = hashlib.md5(open(path, "rb").read()).hexdigest()
    r = read_ppm(path)
    if r is None:
        log(f"  [{label}] NOT P6 md5={md5}")
        return md5, None
    w, h, px = r
    GEOM[0] = (w, h)
    counts = {}
    for i in range(w * h):
        o = i * 3
        if o + 3 > len(px):
            break
        k = (px[o], px[o + 1], px[o + 2])
        counts[k] = counts.get(k, 0) + 1
    top = sorted(counts.items(), key=lambda kv: -kv[1])[:6]
    log(f"  [{label}] {w}x{h} md5={md5} distinct_colours={len(counts)} "
        f"total={w*h}")
    for (cr, cg, cb), n in top:
        log(f"      #{cr:02x}{cg:02x}{cb:02x} = {n:9d}  ({100.0*n/(w*h):5.2f}%)")
    return md5, len(counts)


def shot(q, name):
    p = os.path.join(OUT, f"m13-{PHASE}-{ARCH}-{name}.ppm")
    if not q.screendump(p):
        log(f"  [{name}] screendump refused")
        return None, None
    for _ in range(30):                    # QEMU writes the file asynchronously
        if os.path.exists(p) and os.path.getsize(p) > 0:
            break
        time.sleep(0.2)
    if not os.path.exists(p):
        log(f"  [{name}] no screendump produced")
        return None, None
    res = census(p, name)
    subprocess.run(["sips", "-s", "format", "png", p, "--out",
                    p[:-4] + ".png"], capture_output=True)
    return res


# ------------------------------------------------------------------ main ----
def teardown():
    d("stop", t=90)
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(3)


def main():
    log(f"===== M13 {PHASE} {ARCH} =====  {time.strftime('%F %T')}")
    for p in ("/tmp/leandros-serial.log", QMP_SOCK):
        try:
            os.remove(p)
        except OSError:
            pass

    env = {"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP_SOCK},server,nowait"}
    for attempt in (1, 2):
        teardown()
        out = d("start", ARCH, "uefi", t=360, env=env)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            log(f"[boot] up on attempt {attempt}")
            break
        log(f"[boot] attempt {attempt} failed")
    else:
        log("[boot] NO BOOT — aborting")
        sys.exit(2)

    log(d("login", "root", "root", t=90).strip()[-300:])

    # driver.py is finished with the serial socket; take it for the whole run.
    ser = Serial(tee=f"{OUT}/serial.log")
    ser.drain(1.0)

    # ---- positive control: a command that MUST fail --------------------
    rc, body = run(ser, "nosuchbinary_xyz42", 20)
    log(f"\n[control] `nosuchbinary_xyz42` rc={rc}")
    log("          " + interesting(body)[:300].replace("\n", " | "))
    if rc == 0:
        log("[control] FAILED — a bogus binary reported success; the console "
            "is not executing what we send. Aborting.")
        sys.exit(3)
    log("[control] OK — the console executes and reports failure correctly.")

    # ---- what is actually staged --------------------------------------
    for cmd in (f"ls /usr/share/cosmic 2>&1 | head -20",
                f"ls /usr/share/cosmic/com.system76.CosmicSettings.Shortcuts/v1 2>&1",
                f"ls /usr/share/cosmic/com.system76.CosmicPanel.Panel/v1 2>&1 | head -8"):
        rc, body = run(ser, cmd, 20)
        log(f"\n$ {cmd}\n{interesting(body)[:1500]}")

    # ---- does a bare name resolve the way spawn_command needs it? -------
    # cosmic-comp runs actions as `/bin/sh -c "<command>"` (actions.rs:1065),
    # and /bin/sh is brush, whose exec PATH search has been seen not to fall
    # through /usr/bin -> /bin. If a bare name cannot resolve, a keybinding
    # can fire and still produce nothing — a second, independent cause.
    for pc in ("/bin/sh -c 'uname -s'",
               "/bin/sh -c 'command -v cosmic-launcher'",
               "/bin/sh -c 'command -v cosmic-app-library'",
               "ls -l /bin/cosmic-launcher /bin/cosmic-app-library 2>&1"):
        rc, body = run(ser, pc, 25)
        log(f"\n[pathprobe] {pc} rc={rc}: {interesting(body)[:400]}")

    # ---- user-side control config --------------------------------------
    # mkdir twice: a runtime `mkdir -p` on this f2fs can return 0 without
    # creating the deepest level when several levels are new in one call.
    run(ser, f"mkdir -p {SHORTCUT_DIR}", 20)
    run(ser, f"mkdir -p {SHORTCUT_DIR}", 20)
    run(ser, f"echo '{{(modifiers: [Super], key: \"F9\"): System(Terminal)}}' "
             f"> {SHORTCUT_DIR}/custom", 20)
    run(ser, f"echo '{{Terminal: \"touch /tmp/kb-f9\", "
             f"WebBrowser: \"touch /tmp/kb-b\"}}' "
             f"> {SHORTCUT_DIR}/system_actions", 20)
    rc, body = run(ser, f"cat {SHORTCUT_DIR}/custom {SHORTCUT_DIR}/system_actions", 20)
    log(f"\n[usercfg] rc={rc}\n{interesting(body)[:600]}")
    run(ser, "rm -f /tmp/kb-f9 /tmp/kb-b", 15)

    # ---- pre-stage the whole run as a guest-side choreography ----------
    # The console cannot be typed at reliably once COSMIC is up. Not a FIFO
    # overrun (2 bytes / 90 ms is ~22 B/s): the drops track the kernel's own
    # serial writes — `sh /tmp/s 29` arrived as `sh /tms 29` in the very
    # keystroke window where [DRM-SRV] mmap lines were being printed, while the
    # command just before it, sent during a quiet stretch, was intact. brush
    # also repaints the entire line (with an autosuggestion) per keystroke on
    # the framebuffer console, so every character is expensive.
    #
    # So: type exactly ONE command for the whole measurement, and never write
    # to the console again. The guest script starts the session, waits, prints
    # the log between markers, and then opens fixed windows during which this
    # side injects keys over QMP and reads the witness counts back from the
    # script's own output. Every later step is marker-driven READS only.
    log("\n[stage] writing the guest choreography /tmp/g")
    GUEST = [
        "rm -f /tmp/kb-f9 /tmp/kb-b",
        f"brush /bin/start-cosmic-leandros > {GUEST_LOG} 2>&1 &",
        "i=0",
        f"while [ $i -lt {WAIT_LOOPS} ]; do",
        "  if [ -e /run/user/0/wayland-1 ]; then break; fi",
        "  i=$((i+1)); sleep 2",
        "done",
        "echo M13WAY $i",
        f"sleep {SETTLE}",
        # Everything the guest prints repaints the framebuffer console, which
        # IS the scanout, so a capture taken right after any console write
        # photographs console text rather than the desktop (measured: a
        # 2-colour 96.7% black frame straight after the log dump). Hence the
        # deliberate quiet window here, sampled as a SERIES, and the bulk log
        # dump moved to the very end, after all pixel evidence is taken.
        "echo M13MARK QUIET",
        f"sleep {QUIET}",
        "echo M13MARK INJECT1",
        f"sleep {WINDOW}",
        "echo M13WIT f9 $(ls /tmp/kb-f9 2>/dev/null | wc -l)",
        "echo M13MARK INJECT2",
        f"sleep {WINDOW}",
        "echo M13WIT b $(ls /tmp/kb-b 2>/dev/null | wc -l)",
        "echo M13MARK INJECT3",
        f"sleep {LONGWIN}",
        "echo M13MARK INJECT4",
        f"sleep {LONGWIN}",
        f"echo M13SZ1 $(wc -c < {GUEST_LOG})",
        f"echo M13LINES $(wc -l < {GUEST_LOG})",
        "echo M13WIT f9b $(ls /tmp/kb-f9 /tmp/kb-b 2>/dev/null | wc -l)",
        "echo M13MARK LOGBEGIN",
        f"head -c 200000 {GUEST_LOG}",
        "echo M13MARK LOGEND",
        "echo M13MARK TAILBEGIN",
        f"tail -n 120 {GUEST_LOG}",
        "echo M13MARK TAILEND",
        "echo M13MARK END",
    ]
    run(ser, "rm -f /tmp/g", 20)
    for line in GUEST:
        rc, _ = run(ser, f"echo '{line}' >> /tmp/g", 25)
        if rc != 0:
            log(f"  [stage] WARNING rc={rc} writing {line!r}")
    # Verify byte-for-byte: a single dropped character while staging would
    # silently change what the whole measurement runs.
    rc, body = run(ser, "cat /tmp/g", 60)
    got = [l.strip() for l in interesting(body).split("\n")
           if l.strip() and l.strip() != "cat /tmp/g"]
    want = [l.strip() for l in GUEST]
    if got != want:
        log(f"  [stage] /tmp/g MISMATCH: {len(got)} lines vs {len(want)} expected")
        for i, (a, b) in enumerate(zip(got + [""] * len(want),
                                       want + [""] * len(got))):
            if a != b:
                log(f"    line {i}: got {a!r} want {b!r}")
        log("  [stage] aborting: the choreography is not what we authored.")
        teardown()
        sys.exit(5)
    log(f"  [stage] /tmp/g verified, {len(want)} lines match exactly")

    # ---- QMP: exactly one client, asserted, BEFORE the session starts ----
    q = Qmp()
    if q.f is None:
        log("\n>>> NO QMP: input cannot be injected, so every keybinding "
            "result below would be a false negative. Aborting rather than "
            "publishing them.")
        sys.exit(4)
    log("[qmp] connected")

    # ---- run it; from here the console is read-only ---------------------
    log("\n[session] launching choreography (single typed command)")
    ser.send("brush /tmp/g")

    counts = {}
    results = {}

    def wait(mark, secs):
        m, txt = ser.read_until(re.compile(r"M13MARK " + mark), secs)
        if m is None:
            log(f"  [wait] MARK {mark} never arrived after {secs}s")
        return clean(txt)

    txt = wait("QUIET", WAIT_LOOPS * 2 + SETTLE + 180)
    mm = re.search(r"M13WAY (\d+)", txt)
    loops = int(mm.group(1)) if mm else None
    counts["wayland_wait_loops"] = loops
    log(f"[session] wayland-1 appeared after {loops if loops is not None else '??'}"
        f" x2s polls; socket "
        f"{'present' if loops is not None and loops < WAIT_LOOPS else 'ABSENT'}")

    # Idle series in the quiet window. The first frame is expected to be the
    # console (the QUIET marker just repainted it); whether later frames show
    # the desktop again is itself the measurement of whether the compositor
    # takes the scanout back on its own.
    log("\n----- idle capture series (quiet console) -----")
    for delay, nm in ((3.0, "i0-idle-3s"), (5.0, "i1-idle-8s"),
                      (7.0, "i2-idle-15s"), (10.0, "i3-idle-25s"),
                      (10.0, "i4-idle-35s")):
        ser.pump(delay)
        shot(q, nm)
    if GEOM[0]:
        q.w, q.h = GEOM[0]
        log(f"[qmp] geometry from screendump: {q.w}x{q.h}")

    # ---- CONTROL: user custom binding, independent of /usr/share/cosmic --
    wait("INJECT1", QUIET + 240)
    n0, r0 = q.sent, q.rejected
    q.tap("f9", ("meta_l",))
    log(f"\n[CONTROL Super+F9] injected {q.sent-n0} commands, "
        f"{q.rejected-r0} rejected")
    txt = wait("INJECT2", WINDOW + 180)
    m = re.search(r"M13WIT f9 (\d+)", txt)
    results["CONTROL Super+F9"] = {"witness": "/tmp/kb-f9",
                                   "count": int(m.group(1)) if m else None,
                                   "fired": bool(m and m.group(1) == "1")}
    log(f"[CONTROL Super+F9] witness /tmp/kb-f9 count="
        f"{m.group(1) if m else '??'}")

    # ---- TREATMENT-WITNESS: system `defaults` binding + user action ------
    n0, r0 = q.sent, q.rejected
    q.tap("b", ("meta_l",))
    log(f"\n[TREAT Super+b] injected {q.sent-n0} commands, "
        f"{q.rejected-r0} rejected")
    txt = wait("INJECT3", WINDOW + 180)
    m = re.search(r"M13WIT b (\d+)", txt)
    results["TREAT Super+b"] = {"witness": "/tmp/kb-b",
                                "count": int(m.group(1)) if m else None,
                                "fired": bool(m and m.group(1) == "1")}
    log(f"[TREAT Super+b] witness /tmp/kb-b count={m.group(1) if m else '??'}")

    # ---- TREATMENT-VISUAL: pure upstream, Super+/ -> cosmic-launcher -----
    n0, r0 = q.sent, q.rejected
    q.tap("slash", ("meta_l",))
    log(f"\n[TREAT Super+slash] injected {q.sent-n0} commands, "
        f"{q.rejected-r0} rejected")
    for delay, nm in ((1.5, "t1-slash-1s"), (2.5, "t2-slash-4s"),
                      (4.0, "t3-slash-8s"), (8.0, "t4-slash-16s")):
        ser.pump(delay)
        shot(q, nm)

    wait("INJECT4", LONGWIN + 240)
    n0, r0 = q.sent, q.rejected
    q.tap("a", ("meta_l",))
    log(f"\n[TREAT Super+a] injected {q.sent-n0} commands, "
        f"{q.rejected-r0} rejected")
    for delay, nm in ((2.0, "t5-applib-2s"), (4.0, "t6-applib-6s"),
                      (8.0, "t7-applib-14s")):
        ser.pump(delay)
        shot(q, nm)

    # Pointer motion, as a second, independent input probe on a different
    # virtio device (tablet, not keyboard).
    n0 = q.sent
    for x, y in ((0.50, 0.50), (0.30, 0.40), (0.70, 0.60), (0.50, 0.30)):
        q.move(q.w * x, q.h * y)
        ser.pump(0.5)
    ser.pump(2.0)
    log(f"\n[pointer] injected {q.sent-n0} commands, {q.rejected} rejected total")
    shot(q, "t8-pointer")
    ser.pump(4.0)
    shot(q, "t9-pointer-later")

    # ---- now the bulk log, after every pixel has been taken --------------
    txt = wait("LOGBEGIN", LONGWIN + 240)
    for key, pat in (("log_bytes_after_injection", r"M13SZ1 (\d+)"),
                     ("loglines", r"M13LINES (\d+)"),
                     ("witness_files_present", r"M13WIT f9b (\d+)")):
        m = re.search(pat, txt)
        counts[key] = int(m.group(1)) if m else None
    sess = wait("LOGEND", 900)
    open(f"{OUT}/cosmic-session.log", "w").write(sess)

    log(f"\n----- error census ({len(sess)} chars of session log captured) -----")
    log(f"  log bytes at end         = {counts['log_bytes_after_injection']}")
    log(f"  loglines                 = {counts['loglines']}")
    log(f"  witness files present    = {counts['witness_files_present']} of 2")

    PATTERNS = [
        # The precise, unambiguous marker of the mechanism: cosmic-panel prints
        # the resolved Config, and `system_path: None` IS "the /usr/share
        # directory was not found". Staging should flip these to Some(...).
        ("system_path_none", "system_path: None"),
        ("system_path_some", "system_path: Some"),
        ("system_actions_err", "read system shortcuts config 'system_actions'"),
        ("shortcuts_defaults_err", "shortcuts defaults config error"),
        ("panel_entry_err", "Panel Entry Error"),
        ("panel_noconfigdir", "Panel Entry Error: NoConfigDirectory"),
        ("noconfigdirectory_any", "NoConfigDirectory"),
    ]
    for name, pat in PATTERNS:
        counts[name] = sess.count(pat)
        log(f"  {name:24s} = {counts[name]}")
    for name, pat in PATTERNS:
        hits = [l.strip() for l in sess.split("\n") if pat in l][:3]
        if hits:
            log(f"\n  --- {name} ---")
            for h in hits:
                log(f"    {h[:230]}")

    tail = wait("TAILEND", 400)
    open(f"{OUT}/cosmic-session-tail.log", "w").write(tail)
    for pat in ("launcher", "app-library", "Failed to spawn", "No such file",
                "panic", "Terminal", "WebBrowser"):
        log(f"  tail count {pat!r:18s} = {tail.lower().count(pat.lower())}")


    # ---- verdict --------------------------------------------------------
    log("\n===== RESULT =====")
    log(f"  qmp commands sent = {q.sent}, rejected = {q.rejected}")
    for k, v in results.items():
        state = ("FIRED" if v["fired"] else
                 "no-op" if v["count"] == 0 else f"UNREADABLE({v['count']})")
        log(f"  {k:22s} -> {state:12s} ({v['witness']})")
    for k, v in counts.items():
        log(f"  {k:24s} = {v}")
    if q.rejected:
        log("  WARNING: some QMP commands were rejected; treat key results "
            "as unproven.")

    json.dump({"arch": ARCH, "phase": PHASE, "counts": counts,
               "keys": results, "qmp_sent": q.sent,
               "qmp_rejected": q.rejected},
              open(f"{OUT}/result.json", "w"), indent=2)

    teardown()
    log("done.")


if __name__ == "__main__":
    main()
