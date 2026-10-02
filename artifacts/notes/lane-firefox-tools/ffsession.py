#!/usr/bin/env python3
"""ffsession.py <arch> <tag> [--nofirefox] [--wait S] [--env "K=V ..."] [--url URL] [--prefs "k=v;..."]
--url defaults to about:blank. `file:///tmp/fftest.html` is a test page this
script writes into the guest first (headings, colours, a table, an SVG).
Boot --virgl, serial root login, greeter login as leandro, Super+T cosmic-term,
type `sh /tmp/ffrun.sh` (launches /bin/firefox with output to /tmp/ff.log),
screenshot, collect logs."""
import os, sys, time, subprocess
os.environ.setdefault("LEANDROS_RUN_ID", "firefox")
os.environ.setdefault("LEANDROS_VNC_PORT", "5937")
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../..")))
DRV = f"{REPO}/.claude/skills/run-leandros/driver.py"
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
os.chdir(REPO)
import driver
ARCH = sys.argv[1]; TAG = sys.argv[2]
NOFF = "--nofirefox" in sys.argv
WAIT = int(sys.argv[sys.argv.index("--wait") + 1]) if "--wait" in sys.argv else 150
OUT = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"run-{TAG}")
os.makedirs(OUT, exist_ok=True)
def log(*a): print(time.strftime("%H:%M:%S"), *a, flush=True)
def key(k): driver._monitor_send(f"sendkey {k}"); time.sleep(0.25)
KEYMAP = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", ">": "shift-dot", "&": "shift-7", "_": "shift-minus"}
def typ(s):
    for ch in s: key(KEYMAP.get(ch, ch))
def run(*a, t=400):
    r = subprocess.run([sys.executable, DRV, *a], capture_output=True, text=True, timeout=t)
    log("driver", a[0], "rc", r.returncode, (r.stdout + r.stderr)[-400:])
def sh(c, t=30):
    try:
        o = driver._serial_send(c, timeout=t)
    except BaseException as e:
        o = f"<serial error {e}>"
    return o

run("start", ARCH, "--virgl")
run("login", "root", "root", t=180)
log(sh("ls -l /bin/firefox /usr/lib/firefox/libxul.so /usr/lib/libgtk-3.so.0 /usr/lib/libleandros_ssp.so.1; df 2>/dev/null | head -5"))
# The launcher script, written from the serial root shell so the terminal only
# has to type a short command.
EXTRA = sys.argv[sys.argv.index("--env") + 1] + " " if "--env" in sys.argv else ""
URL = sys.argv[sys.argv.index("--url") + 1] if "--url" in sys.argv else "about:blank"
PAGE = ("<html><title>LeandrOS test page</title><body style=\\\"font-family:sans-serif;background:#eef\\\">"
        "<h1 style=\\\"color:#235\\\">Hello from LeandrOS</h1><p>Firefox rendering a <b>file://</b> page on the GPU.</p>"
        "<div style=\\\"width:300px;height:80px;background:linear-gradient(90deg,red,orange,yellow,green,blue)\\\"></div>"
        "<table border=1><tr><th>arch</th><th>renderer</th></tr><tr><td>guest</td><td>WebRender</td></tr></table>"
        "<svg width=200 height=120><circle cx=60 cy=60 r=50 fill=teal /><rect x=120 y=20 width=70 height=80 fill=purple /></svg>"
        "</body>")
# --prefs "name=value;name=value": a throwaway default-prefs file, for
# bisecting WebRender features. Values are JS literals (true, 0, "str").
PF = "/usr/lib/firefox/defaults/pref/zz-ffsession.js"
sh(f"rm -f {PF}")
if "--prefs" in sys.argv:
    for kv in sys.argv[sys.argv.index("--prefs") + 1].split(";"):
        if kv.strip():
            k, v = kv.split("=", 1)
            sh(f"echo 'pref(\"{k.strip()}\", {v.strip()});' >> {PF}")
    log(sh(f"cat {PF}"))
if URL.startswith("file:///tmp/fftest.html"):
    sh("printf '%s' \"" + PAGE + "\" > /tmp/fftest.html; chmod 644 /tmp/fftest.html; wc -c /tmp/fftest.html")
script = ("echo START >/tmp/ff.log; env | sort >/tmp/ff.env; " + EXTRA +
          "/bin/firefox --no-remote " + URL + " >>/tmp/ff.log 2>&1; echo EXIT=$? >>/tmp/ff.log")
sh(f"printf '%s\\n' '{script}' > /tmp/ffrun.sh; chmod 755 /tmp/ffrun.sh; cat /tmp/ffrun.sh")
time.sleep(40)
typ("leandro"); key("ret")
log("greeter login typed")
time.sleep(50)
run("screenshot", f"{OUT}/desktop.ppm", t=90)
key("meta_l-t"); time.sleep(15)
run("screenshot", f"{OUT}/term.ppm", t=90)
if not NOFF:
    typ("sh /tmp/ffrun.sh"); key("ret")
    log("firefox launched; waiting", WAIT)
    # Hold the serial socket for the whole wait: QEMU drops console output
    # while no client is connected.
    import socket, select
    ss = driver._connect_with_retry(driver.SERIAL_SOCK)
    live = open(f"{OUT}/serial-live.log", "wb")
    t_end = time.time() + WAIT; nxt = time.time() + 30; i = 0
    while time.time() < t_end:
        r = select.select([ss], [], [], 1.0)[0]
        if r:
            d = ss.recv(65536)
            if not d: break
            live.write(d); live.flush()
        if time.time() >= nxt:
            run("screenshot", f"{OUT}/ff-{i}.ppm", t=90); i += 1; nxt = time.time() + 30
    ss.close(); live.close()
run("login", "root", "root", t=180)
o = sh("cat /tmp/ff.log; echo ===ENV; cat /tmp/ff.env; echo ===RUNTIME; ls -la /run/user/1000 /run/user/0 2>&1", t=60)
open(f"{OUT}/ff.log", "w").write(o); log(o[-6000:])
o = sh('hi=$(cut -d" " -f5 /proc/loadavg); for p in $(seq 1 $hi); do readlink /proc/$p/exe 2>/dev/null | grep -q firefox && echo "FFPROC $p $(tr "\\0" " " < /proc/$p/cmdline | cut -c1-120)"; done; echo PSEND', t=90)
open(f"{OUT}/ps.txt", "w").write(o); log(o[-3000:])
subprocess.run(["cp", driver.SERIAL_LOG, f"{OUT}/serial.log"])
subprocess.run(["cp", driver.QEMU_STDERR_LOG, f"{OUT}/qemu-stderr.log"])
run("stop", t=120)
log("DONE", OUT)
