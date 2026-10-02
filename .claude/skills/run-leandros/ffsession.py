#!/usr/bin/env python3
"""ffsession.py <arch> <tag> [--nofirefox] [--wait S] [--env "K=V ..."] [--url URL]
                [--prefs "k=v;..."] [--gpu virgl|venus] [--scale X]
                [--greeter-timeout S] [--desktop-timeout S] [--term-timeout S]
                [--ff-timeout S]

Boot --virgl (--gpu venus: the linux desktop's Venus/zink path), 4G guest RAM
unless LEANDROS_QEMU_MEM says otherwise (at 2G init's memory-pressure guard
kills Firefox during startup on virgl), serial root login, greeter login as
leandro, Super+T cosmic-term, type `sh /tmp/ffrun.sh` (launches /bin/firefox
with output to /tmp/ff.log), screenshot, collect logs.

--url defaults to about:blank. `file:///tmp/fftest.html` is a test page this
script writes into the guest first (headings, colours, a table, an SVG).

Every step waits on an observable condition, polled from the serial root
shell (one held connection, so no console output is dropped between polls),
never on a fixed sleep:
  greeter up    a cosmic-greeter process exists
  desktop up    a cosmic-panel process exists (the user session is drawn)
  terminal up   a cosmic-term process exists
  Firefox ready a Firefox content process exists (`-contentproc`: spawned
                only once the browser window has its first tab)
The *-timeout options cap each wait (seconds, before scaling); a step that
times out is logged and the run continues. `--wait S` (default 150) is how
long Firefox is observed after it is ready (screenshots every 30 s).
All waits are multiplied by driver.wait_scale (x3 on TCG, x1 on HVF/KVM;
--scale or LEANDROS_WAIT_SCALE overrides).
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
os.environ.setdefault("LEANDROS_VNC_PORT", "5937")
os.environ.setdefault("LEANDROS_QEMU_MEM", "4G")
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
    return r.returncode


SER = None


def sh(c, t=60):
    """Run `c` over the held serial connection; return its output."""
    r = driver.serial_run(c, timeout=t * SCALE, arch=ARCH, ser=SER)
    if r["status"] not in ("ok", "rc-file"):
        log(f"sh: {r['status']} for {c[:60]!r}")
    return r["output"]


# One builtin-only pass over /proc (brush's `read` does not fork); the exe
# link names the process. Prints "P <pid> <exe> <ppid>" lines.
PS = ('hi=$(cut -d" " -f5 /proc/loadavg); for p in $(seq 1 $hi); do '
      'e=$(readlink /proc/$p/exe 2>/dev/null) && echo "P $p $e"; done')


def procs():
    res = []
    for line in sh(PS, 90).splitlines():
        m = re.match(r"P (\d+) (\S+)", line.strip())
        if m:
            res.append((int(m[1]), m[2]))
    return res


def firefox_content():
    """Firefox content processes, by cmdline (they share the parent's exe)."""
    o = sh('hi=$(cut -d" " -f5 /proc/loadavg); for p in $(seq 1 $hi); do '
           'case "$(readlink /proc/$p/exe 2>/dev/null)" in *firefox*) '
           'case "$(tr "\\0" " " < /proc/$p/cmdline)" in *-contentproc*) echo "FFC $p";; esac;; esac; done', 90)
    return len(re.findall(r"^FFC \d+", o, re.M))


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


def have(substr):
    return lambda: any(substr in exe for _, exe in procs())


def main():
    global SER
    log(f"ffsession arch={ARCH} accel={driver.accel_kind(ARCH)} gpu={GPU} scale={SCALE}")
    if drv("start", ARCH, f"--{GPU}", t=900) != 0:
        sys.exit("boot failed")
    if drv("login", "root", "root", str(int(60 * SCALE)), t=900) != 0:
        sys.exit("serial login failed")
    SER = driver._Serial()
    log(sh("ls -l /bin/firefox /usr/lib/firefox/libxul.so /usr/lib/libgtk-3.so.0 "
           "/usr/lib/libleandros_ssp.so.1; df 2>/dev/null | head -5"))
    # The launcher script, written from the serial root shell so the terminal
    # only has to type a short command.
    extra = opt("--env", "") + " " if "--env" in A else ""
    url = opt("--url", "about:blank")
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
    if url.startswith("file:///tmp/fftest.html"):
        sh("printf '%s' \"" + page + "\" > /tmp/fftest.html; chmod 644 /tmp/fftest.html; wc -c /tmp/fftest.html")
    script = ("echo START >/tmp/ff.log; env | sort >/tmp/ff.env; " + extra +
              "/bin/firefox --no-remote " + url + " >>/tmp/ff.log 2>&1; echo EXIT=$? >>/tmp/ff.log")
    sh(f"printf '%s\\n' '{script}' > /tmp/ffrun.sh; chmod 755 /tmp/ffrun.sh; cat /tmp/ffrun.sh")

    wait_for("greeter", have("cosmic-greeter"), T_GREETER)
    SER.pump(3 * SCALE)        # first paint of the greeter after its process
    typ("leandro"); key("ret")
    log("greeter login typed")
    wait_for("desktop", have("cosmic-panel"), T_DESKTOP)
    SER.pump(3 * SCALE)
    drv("screenshot", f"{OUT}/desktop.ppm", t=90)
    key("meta_l-t")
    wait_for("terminal", have("cosmic-term"), T_TERM)
    SER.pump(3 * SCALE)        # the terminal's window maps after its process
    drv("screenshot", f"{OUT}/term.ppm", t=90)
    if not NOFF:
        typ("sh /tmp/ffrun.sh"); key("ret")
        log("firefox launched")
        wait_for("firefox", lambda: firefox_content() > 0, T_FF)
        log("observing", WAIT, "s")
        # Hold the serial connection for the whole observation: QEMU drops
        # console output while no client is connected.
        mark = len(SER.buf)
        t_end = time.time() + WAIT
        i = 0
        while time.time() < t_end:
            SER.pump(min(30, max(0, t_end - time.time())))
            drv("screenshot", f"{OUT}/ff-{i}.ppm", t=90); i += 1
        with open(f"{OUT}/serial-live.log", "wb") as f:
            f.write(SER.buf[mark:])
    o = sh("cat /tmp/ff.log; echo ===ENV; cat /tmp/ff.env; echo ===RUNTIME; "
           "ls -la /run/user/1000 /run/user/0 2>&1", 60)
    open(f"{OUT}/ff.log", "w").write(o); log(o[-6000:])
    # The guest has no grep (uutils has none): match with the shell's case.
    o = sh('hi=$(cut -d" " -f5 /proc/loadavg); for p in $(seq 1 $hi); do case "$(readlink /proc/$p/exe 2>/dev/null)" in *firefox*) echo "FFPROC $p $(tr "\\0" " " < /proc/$p/cmdline | cut -c1-120)";; esac; done; echo PSEND', 90)
    open(f"{OUT}/ps.txt", "w").write(o); log(o[-3000:])
    SER.close()
    STEPS["total"] = round(time.time() - T0, 1)
    json.dump(STEPS, open(f"{OUT}/steps.json", "w"), indent=1)
    subprocess.run(["cp", driver.SERIAL_LOG, f"{OUT}/serial.log"])
    subprocess.run(["cp", driver.QEMU_STDERR_LOG, f"{OUT}/qemu-stderr.log"])
    drv("stop", t=120)
    log("DONE", OUT, STEPS)


if __name__ == "__main__":
    main()
