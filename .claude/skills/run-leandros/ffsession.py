#!/usr/bin/env python3
"""ffsession.py <arch> <tag> [--nofirefox] [--wait S] [--env "K=V ..."] [--url URL] [--put HOST:GUEST,...]
                [--prefs "k=v;..."] [--gpu virgl|venus] [--scale X]
                [--greeter-timeout S] [--desktop-timeout S] [--term-timeout S]
                [--ff-timeout S] [--snap] [--post "CMD"] [--wheel]
                [--fresh-profile] [--ffbin PATH] [--pre "CMD"] [--shot-every S] [--freeze-detect N [--freeze-frac F] [--freeze-cmd CMD]]

Boot --virgl (--gpu venus: the linux desktop's Venus/zink path) with the
driver's guest RAM (2G; LEANDROS_QEMU_MEM overrides), serial root login,
greeter login as leandro, Super+T cosmic-term, type `sh /tmp/ffrun.sh` (launches /bin/firefox
with output to /tmp/ff.log), screenshot, collect logs.

--url defaults to about:blank. `file:///tmp/fftest.html` is a test page this
script writes into the guest first (headings, colours, a table, an SVG).

Every step waits on an observable condition, polled from the serial root
shell (one held connection, so no console output is dropped between polls),
never on a fixed sleep:
  greeter up    a cosmic-greeter process exists
  desktop up    a cosmic-panel process exists (the user session is drawn)
  terminal up   a cosmic-term process exists
  Firefox ready >= 3 firefox processes (parent, fork server, and a forked
                child: those start once the browser window exists)
The *-timeout options cap each wait (seconds, before scaling); a step that
times out is logged and the run continues. `--wait S` (default 150) is how
long Firefox is observed after it is ready (screenshots every 30 s).
All waits are multiplied by driver.wait_scale (x3 on TCG, x1 on HVF/KVM;
--scale or LEANDROS_WAIT_SCALE overrides).
`--snap` also saves a filtered /proc/kmemstat (free and page-cache pages,
allocation sites and processes over 10 MiB) as snap-desktop.txt before
Firefox starts and snap-end.txt after the observation.
`--post CMD` runs CMD from the serial root shell after the observation, while
Firefox still runs (e.g. `XDG_RUNTIME_DIR=/run/user/1000 wpctl status`); its
output lands in post.txt.
`--hostpost CMD` runs CMD on the HOST (shell) after the observation, with the
session still live (e.g. `driver.py chord ctrl alt f2` for VT-switch tests);
its output lands in hostpost.txt.
`--wheel` scrolls with the mouse wheel (QMP btn wheel-up/-down, which the
virtio-tablet sends as EV_REL/REL_WHEEL): in cosmic-term after `seq 1 400`,
and in Firefox on a long generated page (the default URL under --wheel).
Each step saves before/after screenshots (wheel-*.ppm) and logs the fraction
of pixels that changed; `wheel.json` holds the numbers.
`--noterm` skips the cosmic-term step (meta_l-t, wait, term.ppm, --wheel terminal
block); without it meta_l-t is re-pressed every 45 s until T_TERM.
`--fresh-profile` deletes leandro's Firefox profile first (a first-run
profile, as on a newly built image).
`--shot-every S` sets the observation screenshot period (default 30 s).
`--freeze-detect N` diffs each observation screenshot with the previous one
and declares the content frozen after N consecutive shots that changed less
than --freeze-frac (default 0.4 %) of the pixels: meant for a playing video
or animation, where every shot differs. On a freeze it saves a Ctrl-T dump
(freeze-ctrlt.txt), runs --freeze-cmd from the serial shell
(freeze-cmd.txt), and with FFSESSION_HOLD=<file> keeps the guest alive until
<file> is removed. frames.json lists (shot, t, changed fraction).
Output: $FFSESSION_OUT (default /tmp/ffsession)/run-<tag>/: ff.log, ps.txt,
screenshots, serial-live.log, serial.log, qemu-stderr.log, steps.json.
"""
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "firefox")
os.environ.setdefault("LEANDROS_VNC_BASE", "5937")
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
os.chdir(REPO)
import driver  # noqa: E402

A = sys.argv
ARCH = A[1]; TAG = A[2]


def opt(name, default, conv=str):
    return conv(A[A.index(name) + 1]) if name in A else default


NOFF = "--nofirefox" in A
NOTERM = "--noterm" in A
CLICK_DEFAULT = False   # --click-term / --no-click-term override
CLICK_TERM = ("--click-term" in A) or (CLICK_DEFAULT and "--no-click-term" not in A)
GPU = opt("--gpu", "virgl")
SCALE = opt("--scale", None, float) or driver.wait_scale(ARCH)
WAIT = opt("--wait", 150, int)
T_GREETER = opt("--greeter-timeout", 120, float) * SCALE
T_DESKTOP = opt("--desktop-timeout", 120, float) * SCALE
T_TERM = opt("--term-timeout", 60, float) * SCALE
T_FF = opt("--ff-timeout", 120, float) * SCALE
OUT = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"run-{TAG}")
os.makedirs(OUT, exist_ok=True)
STEPS = {}
T0 = time.time()


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def key(k):
    driver._monitor_send(f"sendkey {k}")
    time.sleep(0.25)


KEYMAP = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", ">": "shift-dot",
          "&": "shift-7", "_": "shift-minus"}


def typ(s):
    for ch in s:
        key(KEYMAP.get(ch, ch))


def drv(*a, t=400):
    r = subprocess.run([sys.executable, DRV, *a], capture_output=True, text=True, timeout=t)
    log("driver", a[0], "rc", r.returncode, (r.stdout + r.stderr)[-400:])
    global LAST_OUT
    LAST_OUT = r.stdout + r.stderr
    return r.returncode


LAST_OUT = ""


SER = None


def sh(c, t=60):
    """Run `c` over the held serial connection; return its output."""
    r = driver.serial_run(c, timeout=t * SCALE, arch=ARCH, ser=SER)
    if r["status"] not in ("ok", "rc-file"):
        log(f"sh: {r['status']} for {c[:60]!r}")
    return r["output"]


def put_file(src, dst):
    """Copy a small host file into the guest over the serial shell (base64)."""
    import base64
    b = base64.b64encode(open(src, "rb").read()).decode()
    sh(f"rm -f {dst}.b64")
    for i in range(0, len(b), 4000):
        sh(f"printf '%s' '{b[i:i + 4000]}' >> {dst}.b64")
    log(sh(f"base64 -d {dst}.b64 > {dst}; rm -f {dst}.b64; chmod 644 {dst}; wc -c {dst}"))


# Every process's argv, one argument per line, in TWO execs whatever the
# process count. A per-pid loop (`readlink /proc/$p/exe` for p in 1..last pid)
# costs one exec per pid and took over 90 s once Firefox had spawned a few
# hundred pids. argv[0] names the program.
ARGV = 'cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" "\\n"'


def argv_lines():
    return [l.strip() for l in sh(ARGV, 90).splitlines() if l.strip()]


def firefox_procs():
    """Firefox processes. Its children come from the fork server and keep the
    parent's argv (no `-contentproc` to see), so they are counted by argv[0]:
    parent + fork server + at least one forked child (the content/GPU/socket
    processes start once the browser window exists) = ready."""
    return sum(1 for l in argv_lines() if os.path.basename(l) == "firefox")


def brush_count():
    return sum(1 for l in argv_lines() if os.path.basename(l) == "brush")


def launch_firefox(n_brush0):
    """Type `sh /tmp/ffrun.sh` into cosmic-term and check that it ran.
    Keys typed before the terminal's window is mapped and focused are lost
    (x86_64 KVM + virgl: the window maps seconds after the cosmic-term
    process exists; the run then sat at "brush$  /tm" for 9 min with no
    Firefox). So: wait for the terminal's shell, type, confirm /tmp/ff.log
    exists; if not, record a task dump + screenshot and retype the line.
    Both outcomes also save a Ctrl-T baseline and a session-log summary."""
    if n_brush0 is not None:
        wait_for("term_shell", lambda: brush_count() > n_brush0, 60 * SCALE)
        SER.pump(2 * SCALE)
    for n in range(3):
        if n:
            # The keys of the previous try went to cosmic-panel: its Panel and
            # Dock layer surfaces (Top layer, keyboard_interactivity OnDemand)
            # can map seconds AFTER the terminal on x86_64 virgl, and
            # cosmic-comp's map_layer() hands keyboard focus to any such layer
            # surface as it maps, never back (lane kbdfocus9: wl_keyboard.leave
            # on the terminal right after get_layer_surface "Panel"/"Dock").
            # A click on the window gives focus back.
            click_term()
        typ("sh /tmp/ffrun.sh"); key("ret")
        log("firefox launched" + (f" (retype {n})" if n else ""))
        t = time.time()
        while time.time() - t < 30 * SCALE:
            SER.pump(5)
            if "FFLOG2" in sh("[ -e /tmp/ff.log ] && echo FFLOG$((1 + 1))", 30):
                STEPS["ff_retypes"] = n
                ctrl_t("launch-ok-ctrlt")
                session_diag("launch-ok-slog")
                return
        log(f"LAUNCH LOST: /tmp/ff.log missing {30 * SCALE:.0f}s after typing (try {n})")
        drv("screenshot", f"{OUT}/launch-lost-{n}.ppm", t=90)
        if n == 0:
            ctrl_t("launch-lost-ctrlt")
            session_diag("launch-lost-slog")
        key("ctrl-u")    # clear whatever part of the line arrived
    STEPS["ff_retypes"] = "failed"


def wait_for(name, pred, timeout, every=5.0):
    """Poll `pred()` until true or `timeout` s; record the time it took."""
    t = time.time()
    while True:
        try:
            if pred():
                STEPS[name] = round(time.time() - t, 1)
                log(f"{name}: ready after {STEPS[name]}s")
                return True
        except Exception as e:  # noqa: BLE001
            log(f"{name}: poll error {e}")
        if time.time() - t > timeout:
            STEPS[name] = None
            log(f"{name}: NOT ready after {timeout:.0f}s")
            return False
        SER.pump(every)


SNAP = "--snap" in A
SNAPQ = ('cat /proc/kmemstat > /tmp/k1; while read a b c d e f; do case "$a" in '
         'free_pages|pagecache_pages|proc_sum) echo "KM $a $b $c $d";; '
         'proc) [ $((c + d + e)) -gt 2560 ] && echo "KM $a $b $c $d $e $f";; '
         'site) [ "$c" -gt 2560 ] && echo "KM $a $b $c";; esac; done < /tmp/k1')


def snap(tag):
    """--snap: where the guest's memory is (see the docstring)."""
    if SNAP:
        open(f"{OUT}/snap-{tag}.txt", "w").write(sh(SNAPQ, 60))


WHEEL = "--wheel" in A
WHEELRES = {}


def ppm_diff(a, b):
    """Fraction of pixels that differ between two P6 PPMs (None if unreadable)."""
    def rd(f):
        d = open(f, "rb").read()
        parts = d.split(maxsplit=4)
        return int(parts[1]), int(parts[2]), parts[4]
    try:
        wa, ha, pa = rd(a); wb, hb, pb = rd(b)
    except Exception:  # noqa: BLE001
        return None
    if (wa, ha) != (wb, hb):
        return None
    n = wa * ha
    diff = sum(1 for i in range(0, 3 * n, 3) if pa[i:i + 3] != pb[i:i + 3])
    return diff / n


def wheel_step(name, direction, count):
    """Screenshot, scroll `count` detents at screen centre, screenshot, diff."""
    before = f"{OUT}/wheel-{name}-0.ppm"
    after = f"{OUT}/wheel-{name}-1.ppm"
    drv("screenshot", before, t=90)
    drv("wheel", direction, str(count), "16384", "16384", t=60)
    SER.pump(3 * SCALE)
    drv("screenshot", after, t=90)
    frac = ppm_diff(before, after)
    WHEELRES[name] = frac
    log(f"wheel {name}: {direction} x{count} changed {frac if frac is None else round(100 * frac, 2)}% of pixels")


def ppm_size(f):
    parts = open(f, "rb").read(64).split(maxsplit=3)
    return int(parts[1]), int(parts[2])


def click_term():
    """Left-click the middle of the screen (inside the tiled cosmic-term window)
    so the terminal holds keyboard focus before the launch line is typed."""
    drv("screenshot", f"{OUT}/preclick.ppm", t=90)
    w, h = ppm_size(f"{OUT}/preclick.ppm")
    s = driver._qmp_open()
    try:
        driver._qmp_command(s, "input-send-event", {"events": [
            {"type": "abs", "data": {"axis": "x", "value": (w // 2) * 32767 // w}},
            {"type": "abs", "data": {"axis": "y", "value": (h // 2) * 32767 // h}}]})
        time.sleep(0.3)
        for down in (True, False):
            driver._qmp_command(s, "input-send-event", {"events": [
                {"type": "btn", "data": {"down": down, "button": "left"}}]})
            time.sleep(0.1)
    finally:
        s.close()
    SER.pump(2 * SCALE)
    log("clicked the terminal window")


def click_step(name, where):
    """Pointer-button control for --wheel: left-click at a screen pixel,
    screenshot before/after. `where` maps framebuffer width to (px, py): the
    mode differs per arch (1280x800 on aarch64, 1920x1080 on x86_64)."""
    before = f"{OUT}/click-{name}-0.ppm"
    after = f"{OUT}/click-{name}-1.ppm"
    drv("screenshot", before, t=90)
    w, h = ppm_size(before)
    px, py = where.get(w, (w // 2, h // 2))
    s = driver._qmp_open()
    try:
        driver._qmp_command(s, "input-send-event", {"events": [
            {"type": "abs", "data": {"axis": "x", "value": px * 32767 // w}},
            {"type": "abs", "data": {"axis": "y", "value": py * 32767 // h}}]})
        time.sleep(0.3)
        for down in (True, False):
            driver._qmp_command(s, "input-send-event", {"events": [
                {"type": "btn", "data": {"down": down, "button": "left"}}]})
            time.sleep(0.1)
    finally:
        s.close()
    SER.pump(3 * SCALE)
    drv("screenshot", after, t=90)
    frac = ppm_diff(before, after)
    WHEELRES[f"click-{name}"] = frac
    log(f"click {name}: changed {frac if frac is None else round(100 * frac, 2)}% of pixels")


def have(name):
    return lambda: any(os.path.basename(l).startswith(name) for l in argv_lines())


SHOT_EVERY = opt("--shot-every", 30, float)
FREEZE_N = opt("--freeze-detect", 0, int)
FREEZE_FRAC = opt("--freeze-frac", 0.004, float)
FRAMES = []


def freeze_diag(i):
    """--freeze-detect: the content area stopped changing. Ctrl-T task dump,
    the --freeze-cmd output, then (FFSESSION_HOLD=<file>) keep QEMU and the
    serial connection alive while <file> exists, so the frozen guest can be
    inspected from outside (driver.py cmd/screenshot, a second shell)."""
    # First watch, without touching the guest, whether it recovers by itself
    # (--freeze-watch S, default 0), then the dump, then whether the dump
    # (which holds the run queue for a while) unstuck it.
    w = opt("--freeze-watch", 0, float)
    if w:
        recovered = watch_shots(i, w, "pre")
        STEPS["self_recovered"] = recovered
        if recovered is not None:
            log(f"FREEZE self-recovered after {recovered} s without any guest input")
            return
        log(f"FREEZE persisted {w:.0f} s with no input")
    ctrl_t("freeze-ctrlt")
    if w:
        r = watch_shots(i, 40, "post")
        STEPS["recovered_after_ctrlt"] = r
        log(f"after Ctrl-T: {'recovered after %s s' % r if r is not None else 'still frozen'}")
    if "--freeze-cmd" in A:
        o = sh(opt("--freeze-cmd", ""), 300)
        open(f"{OUT}/freeze-cmd.txt", "w").write(o)
    hold = os.environ.get("FFSESSION_HOLD")
    if hold:
        open(hold, "w").write(f"{OUT} frozen at shot {i}\n")
        log(f"HOLD: frozen guest kept alive until {hold} is removed")
        t = time.time()
        k = 0
        while os.path.exists(hold) and time.time() - t < 4 * 3600:
            SER.pump(5)
            # <hold>.cmd: one serial-shell command (or "^T" for a task dump)
            # to run while held; the output lands in <hold>.out.<k>.
            c = hold + ".cmd"
            if os.path.exists(c):
                cmd = open(c).read().strip()
                os.unlink(c)
                if cmd == "^T":
                    ctrl_t(f"hold-ctrlt-{k}")
                    o = open(f"{OUT}/hold-ctrlt-{k}.txt", "rb").read().decode("latin-1")
                else:
                    o = sh(cmd, 300)
                open(f"{hold}.out.{k}", "w").write(o)
                k += 1
        log("HOLD released")


def watch_shots(i, secs, tag):
    """Screenshots every 10 s for `secs`, each diffed with the frozen shot
    ff-<i>; returns the seconds until one differs (content moved again) or
    None."""
    t0 = time.time()
    k = 0
    while time.time() - t0 < secs:
        SER.pump(10)
        f = f"{OUT}/{tag}-{k}.ppm"
        drv("screenshot", f, t=90)
        frac = ppm_diff(f"{OUT}/ff-{i}.ppm", f)
        log(f"watch {tag}-{k}: changed {frac}")
        if frac is not None and frac >= FREEZE_FRAC:
            return round(time.time() - t0)
        k += 1
    return None


def ctrl_t(name):
    """Ctrl-T task dump into OUT/<name>.txt. dump_tasks gives up when the run
    queue lock is busy ("run queue busy, try again"): retry a few times."""
    for _ in range(6):
        m = len(SER.buf)
        SER.send(b"\x14")
        # Until the dump has gone quiet for 3 s (it is a few hundred lines).
        t_end = time.time() + 30 * SCALE
        n = -1
        while time.time() < t_end and len(SER.buf) != n:
            n = len(SER.buf)
            SER.pump(3)
        got = SER.buf[m:]
        if b"run queue busy" not in got or b"[TASKS] pid=" in got:
            break
    open(f"{OUT}/{name}.txt", "wb").write(SER.buf[m:])


# Keyboard-path trace (lane kbdfocus9). The session-env RUST_LOG never showed
# up in the session log: cosmic-session does not relay cosmic-comp's stderr.
# So the harness wraps the two binaries itself (root shell, before login):
# /bin/cosmic-comp -> RUST_LOG focus/keyboard trace, stderr to its own file;
# /bin/cosmic-term -> WAYLAND_DEBUG=1 (client side: wl_keyboard enter/keymap/key).
# The real binaries move to /usr/libexec/lrf/ so argv[0] keeps its basename.
WLTRACE = "--wltrace" in A   # opt-in: the wrappers add load and edit /bin
COMP_RUST_LOG = ("info,cosmic_comp::input=trace,cosmic_comp::shell::focus=trace,"
                 "smithay::input::keyboard=trace")


def install_wraps():
    for name, extra in (("cosmic-comp", f"export RUST_LOG='{COMP_RUST_LOG}' WAYLAND_DEBUG=server"),
                        ("cosmic-term", "export WAYLAND_DEBUG=1")):
        src = f"{OUT}/wrap-{name}"
        open(src, "w").write("#!/bin/sh\n"
                             f"env > /tmp/{name}-env.$$ 2>/dev/null\n"
                             f"{extra}\n"
                             f"exec /usr/libexec/lrf/{name} \"$@\" >>/tmp/{name}-$(id -u).log 2>&1\n")
        sh(f"mkdir -p /usr/libexec/lrf; [ -e /usr/libexec/lrf/{name} ] || mv /bin/{name} /usr/libexec/lrf/{name}")
        put_file(src, f"/bin/{name}")
        log(sh(f"chmod 755 /bin/{name}; ls -l /usr/libexec/lrf/{name} /bin/{name} | cut -c1-120"))


PORTALSAMP = "--portalsamp" in A
# --portalsamp (lane portalstart): root sampler started before login; every 0.1 s it
# writes the uptime and the /proc/sysstat 'w' line (state, syscall, port) of every
# thread of the xdg-*/busd/dbus processes into /tmp/pls.log, plus a 'P' line per new
# process (tgid, comm, argv) so the host can rebuild a per-step timeline.
PORTAL_SAMP = r"""S=/tmp/pls.log; : >$S; SEEN=" "
while :; do read u x </proc/uptime; W=""
for d in /proc/[0-9]*; do p=${d#/proc/}; read c <$d/comm 2>/dev/null || continue
case $c in xdg*|busd*|dbus*|cosmic-sess*|cosmic-set*|pop-*|leandros*)
 W="$W|$p"; case "$SEEN" in *" $p "*) ;; *) SEEN="$SEEN$p "; echo "P $u $p $c $(head -c 150 $d/cmdline | tr '\\0' ' ')" >>$S;; esac;; esac; done
echo "@ $u" >>$S
[ -n "$W" ] && grep -E "^w [0-9]+ (${W#|}) " /proc/sysstat | cut -c1-80 >>$S
sleep 0.1; done
"""


def install_portal_samp():
    src = f"{OUT}/plsamp.sh"
    open(src, "w").write(PORTAL_SAMP)
    put_file(src, "/tmp/plsamp.sh")
    log(sh("(/bin/sh /tmp/plsamp.sh </dev/null >/dev/null 2>&1 &); sleep 1; ls -l /tmp/pls.log"))


PANELTRACE = "--paneltrace" in A or os.environ.get("FFS_PANELTRACE") == "1"
# --paneltrace (lane panellate): when does cosmic-panel create and map its layer
# surfaces?  A /bin/cosmic-panel wrapper logs its start (uptime + realtime),
# runs the panel with WAYLAND_DEBUG=1 and samples the panel's threads in
# /proc/sysstat ('w' lines: state, syscall, time in it) every 0.2 s for 30 s.
PANEL_SAMP = r"""P=$1; S=$2; i=0
# Also sample the bus daemons' threads (lane panelprof): the panel's first
# step is a blocking session-bus connect, so a stall shows up in busd too.
B=$P; for d in /proc/[0-9]*; do read c <$d/comm; [ "$c" = busd ] && B="$B|${d#/proc/}"; done 2>/dev/null
echo "busd tgids: $B" >>$S
while [ $i -lt 120 ]; do read u x </proc/uptime; echo "@ $u" >>$S
if [ $((i%8)) = 0 ]; then grep -E "^(w [0-9]+|t [0-9]+|s) ($B) " /proc/sysstat; else grep -E "^w [0-9]+ ($B) " /proc/sysstat; fi | cut -c1-110 >>$S; i=$((i+1)); sleep 0.15; done
echo END >>$S; grep -E "^([st] [0-9]+|s) $P " /proc/sysstat >>$S
"""
PANEL_WRAP = r"""#!/bin/sh
U=$(id -u); S=/tmp/plsamp-$U.log
read u x </proc/uptime; echo "start up=$u x rt=$(date +%s%N) pid=$$" >>$S
/bin/sh /usr/libexec/lrf/plsamp.sh $$ $S </dev/null >/dev/null 2>>$S &
export WAYLAND_DEBUG=1
[ -n "$PANEL_RUST_LOG" ] && export RUST_LOG="$PANEL_RUST_LOG"
[ -r /usr/libexec/lrf/panel-rust-log ] && export RUST_LOG="$(cat /usr/libexec/lrf/panel-rust-log)"
exec /usr/libexec/lrf/cosmic-panel "$@" >>/tmp/cosmic-panel-$U.log 2>&1
"""


# Default on under --paneltrace: zbus=trace timestamps the session-bus connect
# (SpaceContainer::new blocks on it); FFS_PANEL_RUST_LOG=off disables it.
PANEL_RUST_LOG = os.environ.get("FFS_PANEL_RUST_LOG", "info,zbus=trace")
PANEL_RUST_LOG = "" if PANEL_RUST_LOG == "off" else PANEL_RUST_LOG


def install_panel_wrap():
    src = f"{OUT}/wrap-cosmic-panel"
    open(src, "w").write(PANEL_WRAP)
    sh("mkdir -p /usr/libexec/lrf; [ -e /usr/libexec/lrf/cosmic-panel ] || mv /bin/cosmic-panel /usr/libexec/lrf/cosmic-panel")
    put_file(src, "/bin/cosmic-panel")
    open(f"{OUT}/plsamp.sh", "w").write(PANEL_SAMP)
    put_file(f"{OUT}/plsamp.sh", "/usr/libexec/lrf/plsamp.sh")
    sh("chmod 755 /bin/cosmic-panel")
    # FFS_PANEL_RUST_LOG=<filter>: the panel's own tracing (e.g. zbus=trace)
    # timestamps each startup step; the session env does not reach the panel.
    rl = PANEL_RUST_LOG
    sh(f"echo '{rl}' >/usr/libexec/lrf/panel-rust-log" if rl else "rm -f /usr/libexec/lrf/panel-rust-log")


def panel_diag():
    """Save the panel's start, layer-surface and first-commit times (uptime s)."""
    L = "/tmp/cosmic-panel-1000.log"
    c = ("cat /tmp/plsamp-1000.log; echo ===UP; cat /proc/uptime; echo ===WL; "
         f"grep -m2 -n 'get_registry' {L} | cut -c1-160; grep -m4 -n 'get_layer_surface' {L} | cut -c1-160; "
         f"grep -m4 -n 'zwlr_layer_surface_v1[#@][0-9]+[.]configure' {L} | cut -c1-160; "
         f"grep -m6 -n -e '-> wl_surface[#@][0-9]+[.]attach' {L} | cut -c1-160; "
         f"grep -m3 -n 'zwp_linux_dmabuf' {L} | cut -c1-160; echo panel_log_bytes=$(wc -c <{L}); "
         f"echo ===PLOG; grep -m40 -v -e '->' -e ' wl_' -e '@' {L} | cut -c1-200; "
         "echo ===START; for d in /proc/[0-9]*; do read c <$d/comm; case $c in cosmic*|greetd*|Xwayland*) "
         "echo \"$c $(cut -d' ' -f22 $d/stat)\";; esac; done 2>/dev/null")
    o = sh(c, 120)
    open(f"{OUT}/paneltrace.txt", "w").write(o)
    if PANEL_RUST_LOG:
        open(f"{OUT}/panel-rustlog.txt", "w").write(
            sh(f"grep -v -e ' wl_' -e '-> ' {L} | head -n 250 | cut -c1-200", 120))
    try:
        st = re.search(r"start up=([\d.]+) \S+ rt=(\d+)", o)
        up0, rt0 = float(st.group(1)), int(st.group(2))
        def to_up(stamp):
            # libwayland and wayland-backend >= 0.3.11 both print
            # (realtime us mod 2^32)/1000; very old wayland-rs printed realtime
            # s%10000. Try the u32 form FIRST: both readings often land within
            # 600 s of start, and taking s%10000 first reported +24..32 s for a
            # panel that called get_registry +1.3 s after exec (panellate4).
            for cand, per in (((rt0 // 1000) % 2**32 / 1e6, 2**32 / 1e6), (rt0 / 1e9 % 10000, 10000)):
                d = (stamp / 1000 - cand) % per
                if d < 600:
                    return round(up0 + d, 2)
        def first(pat):
            m = re.search(r"\[\s*(\d+\.\d+)\][^\n]*" + pat, o)
            return to_up(float(m.group(1))) if m else None
        starts = {}
        for m in re.finditer(r"^(cosmic[\w-]*) (\d+)$", o, re.M):
            starts.setdefault(m.group(1), round(int(m.group(2)) / 100, 2))
        r = {"panel_start": up0, "registry": first("get_registry"), "layer": first("get_layer_surface"),
             "configure": first(r"zwlr_layer_surface_v1[#@]\d+\.configure"), "attach": first(r"-> wl_surface[#@]\d+\.attach"),
             "procs": starts}
        # Guest-wide slowness indicator: mean period of the sampler loop (one
        # uptime read + grep + sleep 0.15) over its first 20 samples. About
        # 0.15-0.25 s on a quiet box; 0.6 s+ when the box host is contended
        # (then block reads take 100-270 ms and panel start takes 9-10 s).
        ups = [float(x) for x in re.findall(r"^@ ([\d.]+)$", o, re.M)][:21]
        if len(ups) > 1:
            r["samp_period"] = round((ups[-1] - ups[0]) / (len(ups) - 1), 2)
        STEPS["paneltrace"] = r
        log("PANELTRACE " + json.dumps(r))
    except (AttributeError, ValueError) as e:
        log(f"PANELTRACE unparsed: {e!r}")


def session_diag(name):
    """Save a focus/keystroke summary of the COSMIC session log to OUT/<name>.txt."""
    c = ("L=/tmp/cosmic-session-1000.log; echo bytes=$(wc -c <$L); "
         "for p in 'key keycode' 'Focusing window' 'Restoring focus' "
         "'focus fixup' 'restore focus to previous popup grab' 'grab keyboard'; do "
         "echo \"$p=$(grep -c \"$p\" $L)\"; done; "
         "grep -E 'Focusing window|focus fixup|Restoring focus|popup grab' $L | tail -n 20 | cut -c1-300; "
         "echo ---warn; "
         "grep -iE 'warn|error|panic' $L | grep -v 'keycode' | tail -n 40 | cut -c1-300; "
         "echo ---tail; tail -n 25 $L | cut -c1-300")
    if WLTRACE:
        c += ("; echo ===COMPENV; cat /tmp/cosmic-comp-env.* /tmp/cosmic-term-env.* | grep -E '^(RUST_LOG|WAYLAND_DEBUG)='; "
              "C=/tmp/cosmic-comp-1000.log; T=/tmp/cosmic-term-1000.log; "
              "echo comp_bytes=$(wc -c <$C) term_bytes=$(wc -c <$T); "
              "for p in 'Handling keystroke' 'Input forwarded to client' 'No client currently focused' "
              "'intercepted by filter' 'Focus set to new surface' 'Focusing window' 'focus fixup'; do "
              "echo \"comp $p=$(grep -c \"$p\" $C)\"; done; "
              "echo ===COMPFOCUS; grep -E 'Focus set|Focusing window|focus fixup|Restoring focus|No client|intercepted|"
              "get_layer_surface|set_keyboard_interactivity|wl_keyboard#[0-9]+[.](enter|leave)|zwlr_layer_surface_v1#[0-9]+[.]destroy' $C "
              "| grep -v ' key ' | tail -n 80 | cut -c1-300; "
              "echo ===TERMKBD; for p in 'wl_keyboard#[0-9]*[.]enter' 'wl_keyboard#[0-9]*[.]leave' "
              "'wl_keyboard#[0-9]*[.]keymap' 'wl_keyboard#[0-9]*[.]key[(]' 'wl_keyboard#[0-9]*[.]modifiers'; do "
              "echo \"term $p=$(grep -c \"$p\" $T)\"; done; "
              "grep -E 'wl_keyboard|wl_seat' $T | grep -v '[.]key[(]' | tail -n 15 | cut -c1-200; "
              "grep 'wl_keyboard#[0-9]*[.]key[(]' $T | tail -n 6 | cut -c1-200; "
              "echo ===TERMTAIL; grep -v 'wl_callback\\|frame\\|wl_surface\\|wl_buffer\\|damage' $T | tail -n 20 | cut -c1-200")
    open(f"{OUT}/{name}.txt", "w").write(sh(c, 90))


def main():
    global SER
    log(f"ffsession arch={ARCH} accel={driver.accel_kind(ARCH)} gpu={GPU} scale={SCALE}")
    rc = drv("start", ARCH, f"--{GPU}", t=900)
    if rc != 0 and "QEMU exited" in LAST_OUT:
        log("start: QEMU exited during boot, retrying once")
        drv("stop", t=60)
        rc = drv("start", ARCH, f"--{GPU}", t=900)
    if rc != 0:
        # A start that times out leaves QEMU up, and every later run under this
        # id then fails with "QEMU already running".
        drv("stop", t=60)
        sys.exit("boot failed")
    if drv("login", "root", "root", str(int(60 * SCALE)), t=900) != 0:
        sys.exit("serial login failed")
    SER = driver._Serial()
    log(sh("ls -l /bin/firefox /usr/lib/firefox/libxul.so /usr/lib/libgtk-3.so.0 "
           "/usr/lib/libleandros_ssp.so.1; df 2>/dev/null | head -5"))
    # The launcher script, written from the serial root shell so the terminal
    # only has to type a short command.
    extra = opt("--env", "") + " " if "--env" in A else ""
    url = opt("--url", "file:///tmp/fflong.html" if WHEEL else "about:blank")
    page = ("<html><title>LeandrOS test page</title><body style=\\\"font-family:sans-serif;background:#eef\\\">"
            "<h1 style=\\\"color:#235\\\">Hello from LeandrOS</h1><p>Firefox rendering a <b>file://</b> page on the GPU.</p>"
            "<div style=\\\"width:300px;height:80px;background:linear-gradient(90deg,red,orange,yellow,green,blue)\\\"></div>"
            "<table border=1><tr><th>arch</th><th>renderer</th></tr><tr><td>guest</td><td>WebRender</td></tr></table>"
            "<svg width=200 height=120><circle cx=60 cy=60 r=50 fill=teal /><rect x=120 y=20 width=70 height=80 fill=purple /></svg>"
            "</body>")
    # --prefs "name=value;name=value": a throwaway default-prefs file, for
    # bisecting WebRender features. Values are JS literals (true, 0, "str").
    pf = "/usr/lib/firefox/defaults/pref/zz-ffsession.js"
    sh(f"rm -f {pf}")
    if "--prefs" in A:
        for kv in opt("--prefs", "").split(";"):
            if kv.strip():
                k, v = kv.split("=", 1)
                sh(f"echo 'pref(\"{k.strip()}\", {v.strip()});' >> {pf}")
        log(sh(f"cat {pf}"))
    # --put "HOST_PATH:GUEST_PATH[,...]": copy small host files (a test page,
    # a media sample) into the guest over the serial shell, base64 in chunks.
    if "--put" in A:
        for spec in opt("--put", "").split(","):
            src, dst = spec.split(":", 1)
            put_file(src, dst)
    if url.startswith("file:///tmp/fflong.html"):
        # 300 numbered, coloured rows: any scroll moves every visible row.
        rows = "".join(f'<p style="background:hsl({k * 37 % 360},70%,80%)">Row {k}</p>'
                       for k in range(1, 301))
        src = f"{OUT}/fflong.html"
        open(src, "w").write('<html><title>wheel test</title><body style="font:20px sans-serif">'
                             + rows + "</body></html>")
        put_file(src, "/tmp/fflong.html")
    if url.startswith("file:///tmp/fftest.html"):
        sh("printf '%s' \"" + page + "\" > /tmp/fftest.html; chmod 644 /tmp/fftest.html; wc -c /tmp/fftest.html")
    if "--pre" in A:
        # --pre CMD: run from the serial root shell just before the launcher
        # script is written (e.g. stage a distribution policy, remove a pref file).
        log(sh(opt("--pre", ""), 60))
    script = ("unset WAYLAND_DEBUG; export RUST_LOG=info; echo START >/tmp/ff.log; env | sort >/tmp/ff.env; " + extra +
              opt("--ffbin", "/bin/firefox") + " --no-remote " + url + " >>/tmp/ff.log 2>&1; echo EXIT=$? >>/tmp/ff.log")
    sh(f"printf '%s\\n' '{script}' > /tmp/ffrun.sh; chmod 755 /tmp/ffrun.sh; cat /tmp/ffrun.sh")
    if "--fresh-profile" in A:
        # A new Firefox profile, as on the first run after an image build:
        # first-run work (Remote Settings sync, certificate downloads,
        # IndexedDB/cache churn) is part of what is under test.
        log(sh("rm -rf /home/leandro/.mozilla /home/leandro/.cache/mozilla; ls -d /home/leandro/.mozilla 2>&1"))

    # Compositor keyboard-focus / input trace for the session (see the profile).
    sh("mkdir -p /etc/leandros; printf '%s\\n' 'RUST_LOG=info,cosmic_comp::input=trace,cosmic_comp::shell::focus=trace' > /etc/leandros/session-env")
    if WLTRACE:
        install_wraps()
    if PANELTRACE:
        install_panel_wrap()
    if PORTALSAMP:
        install_portal_samp()
    if not wait_for("greeter", have("cosmic-greeter"), T_GREETER):
        # No greeter: keep what shows why (the compositor's log, every process's
        # scheduler state, where the CPUs are) before the run goes on and is torn down.
        open(f"{OUT}/greeterfail.txt", "w").write(sh(
            "tail -n 60 /var/log/greetd.log; echo ===STAT; cat /proc/[0-9]*/stat; "
            "echo ===STATUS; for p in /proc/[0-9]*; do head -c 400 $p/cmdline 2>/dev/null | tr '\\0' ' '; "
            "echo; cat $p/status 2>/dev/null | head -n 12; done; echo ===WAKE; cat /proc/wakestat; "
            "echo ===POLLFDS; cat /proc/pollfds; echo ===SYS; cat /proc/sysstat; echo ===PC; cat /proc/pcsample; echo ===VT; cat /proc/vtstat", 300))
    SER.pump(float(os.environ.get("FFS_GREETER_PAINT", 3)) * SCALE)  # greeter first paint (virgl: ~90 s)
    typ("leandro"); key("ret")
    log("greeter login typed")
    wait_for("desktop", have("cosmic-panel"), T_DESKTOP)
    SER.pump(3 * SCALE)
    drv("screenshot", f"{OUT}/desktop.ppm", t=90)
    n_brush0 = None
    if NOTERM:
        log("terminal: skipped (--noterm)")
        STEPS["terminal"] = "skipped"
    else:
        t_term = time.time()
        n_brush0 = brush_count()
        key("meta_l-t")
        n = 0
        while True:
            left = T_TERM - (time.time() - t_term)
            if wait_for("terminal", have("cosmic-term"), min(45 * SCALE, max(left, 1))):
                STEPS["terminal"] = round(time.time() - t_term, 1)
                break
            if T_TERM - (time.time() - t_term) <= 0:
                STEPS["terminal"] = None
                break
            n += 1
            log(f"terminal: meta_l-t retry {n}")
            key("meta_l-t")
        STEPS["term_retries"] = n
        SER.pump(3 * SCALE)        # the terminal's window maps after its process
        drv("screenshot", f"{OUT}/term.ppm", t=90)
        if WHEEL:
            typ("seq 1 400"); key("ret")
            SER.pump(3 * SCALE)
            wheel_step("term-up", "up", 15)
            wheel_step("term-down", "down", 15)
            # cosmic-term's "View" menu: a click must still open it.
            click_step("term-view-menu", {1280: (352, 141), 1920: (586, 176)})
            key("esc")
    snap("desktop")
    if not NOFF:
        if CLICK_TERM and not NOTERM:
            click_term()
        launch_firefox(n_brush0)
        wait_for("firefox", lambda: firefox_procs() >= 3, T_FF)
        if WHEEL:
            SER.pump(20 * SCALE)   # page load and first paint
            wheel_step("ff-down", "down", 10)
            wheel_step("ff-up", "up", 5)
        log("observing", WAIT, "s")
        # Hold the serial connection for the whole observation: QEMU drops
        # console output while no client is connected.
        mark = len(SER.buf)
        t_obs = time.time()
        t_end = t_obs + WAIT
        i = 0
        static = 0
        frozen = False
        while time.time() < t_end:
            SER.pump(min(SHOT_EVERY, max(0, t_end - time.time())))
            drv("screenshot", f"{OUT}/ff-{i}.ppm", t=90)
            if FREEZE_N and i > 0:
                frac = ppm_diff(f"{OUT}/ff-{i - 1}.ppm", f"{OUT}/ff-{i}.ppm")
                t_rel = round(time.time() - t_obs)
                FRAMES.append((i, t_rel, frac))
                # A playing 1080p test pattern changes most of the screen
                # between two shots; a frozen content area leaves only the
                # panel clock (well under 1 %).
                static = static + 1 if frac is not None and frac < FREEZE_FRAC else 0
                if static >= FREEZE_N and not frozen:
                    frozen = True
                    STEPS["freeze_at_s"] = t_rel - static * SHOT_EVERY
                    log(f"FREEZE: {static} identical shots, from about t={STEPS['freeze_at_s']} s")
                    freeze_diag(i)
            i += 1
        if FREEZE_N:
            STEPS["frozen"] = frozen
            json.dump(FRAMES, open(f"{OUT}/frames.json", "w"))
        with open(f"{OUT}/serial-live.log", "wb") as f:
            f.write(SER.buf[mark:])
    if "--post" in A:
        o = sh(opt("--post", ""), opt("--post-timeout", 120, float))
        open(f"{OUT}/post.txt", "w").write(o); log(o[-4000:])
    if "--hostpost" in A:
        # Host-side command (chords, screenshots) while the session is live.
        hp = subprocess.run(opt("--hostpost", ""), shell=True, capture_output=True,
                            text=True, timeout=1500)
        open(f"{OUT}/hostpost.txt", "w").write(hp.stdout + hp.stderr); log(hp.stdout[-4000:])
        o = sh("cat /proc/vtstat", 30)  # the guest has no grep; filter on the host
        open(f"{OUT}/vtstat-end.txt", "w").write(o)
        log("VTSTAT", " | ".join(l for l in o.splitlines() if "vt_handoff" in l))
    snap("end")
    if PANELTRACE:
        panel_diag()
    o = sh("cat /tmp/ff.log; echo ===ENV; cat /tmp/ff.env; echo ===RUNTIME; "
           "ls -la /run/user/1000 /run/user/0 2>&1", 60)
    open(f"{OUT}/ff.log", "w").write(o); log(o[-6000:])
    # Every argv, filtered here (the guest has no grep).
    # `head` on many files prints a "==> file <==" header before each: one
    # exec for every process's argv, with the pid attached.
    o = sh('head -c 400 /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" " "', 90)
    open(f"{OUT}/ps.txt", "w").write(o)
    ff = [l for l in o.splitlines() if "firefox" in l and not l.startswith("==>")]
    log(f"{len(ff)} firefox processes")
    log("\n".join(l[:160] for l in ff)[-3000:])
    SER.close()
    if WHEEL:
        json.dump(WHEELRES, open(f"{OUT}/wheel.json", "w"), indent=1)
    STEPS["total"] = round(time.time() - T0, 1)
    json.dump(STEPS, open(f"{OUT}/steps.json", "w"), indent=1)
    log("STEPS " + json.dumps(STEPS))
    subprocess.run(["cp", driver.SERIAL_LOG, f"{OUT}/serial.log"])
    subprocess.run(["cp", driver.QEMU_STDERR_LOG, f"{OUT}/qemu-stderr.log"])
    drv("stop", t=120)
    log("DONE", OUT, STEPS)


if __name__ == "__main__":
    main()
