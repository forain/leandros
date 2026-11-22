#!/usr/bin/env python3
"""inotifydesk.py <arch> <outdir> <cmdline>: COSMIC idle sampling (L5)."""
import imgclone
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "inotifydesk")
os.environ.setdefault("LEANDROS_VNC_BASE", "5943")
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
os.chdir(REPO)
import driver  # noqa: E402

KEYMAP = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", "_": "shift-minus"}


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def drv(*a, t=400):
    r = subprocess.run([sys.executable, DRV, *a], capture_output=True, text=True, timeout=t)
    log("driver", a[0], "rc", r.returncode, (r.stdout + r.stderr)[-300:].strip())
    return r.returncode


def key(k):
    driver._monitor_send(f"sendkey {k}")
    time.sleep(0.25)


def typ(s):
    for ch in s:
        if ch.isupper():
            key("shift-" + ch.lower())
        else:
            key(KEYMAP.get(ch, ch))


def ppm_size(f):
    with open(f, "rb") as fh:
        fh.readline()
        line = fh.readline()
        while line.startswith(b"#"):
            line = fh.readline()
        w, h = line.split()
        return int(w), int(h)


def shot(path):
    ppm = path[:-4] + ".ppm" if path.endswith(".png") else path
    drv("screenshot", ppm, t=120)
    return ppm


def click(x, y, w, h, button="left"):
    s = driver._qmp_open()
    try:
        driver._qmp_command(s, "input-send-event", {"events": [
            {"type": "abs", "data": {"axis": "x", "value": x * 32767 // w}},
            {"type": "abs", "data": {"axis": "y", "value": y * 32767 // h}}]})
        time.sleep(0.3)
        for down in (True, False):
            driver._qmp_command(s, "input-send-event", {"events": [
                {"type": "btn", "data": {"down": down, "button": button}}]})
            time.sleep(0.12)
    finally:
        s.close()


def session_logs(sh):
    """The session's log (cosmic-session relays every component's stdout and
    stderr, busd-activated services included, into greetd.log), ANSI-stripped."""
    return re.sub(r"\x1b\[[0-9;]*m|\\x1b\[[0-9;]*m", "", sh("cat /var/log/greetd.log", 180))


def clone_path(arch, what="boot"):
    return f"/tmp/leandros-{os.environ['LEANDROS_RUN_ID']}-inotifydesk-{what}-{arch}.img"


def main():
    arch, out, cmdline = sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else ""
    idle = int(os.environ.get("IDLE_SECS", "120"))
    os.makedirs(out, exist_ok=True)
    scale = driver.wait_scale(arch)
    for what, src, var in (("boot", f"leandros-limine-{arch}.img", "LEANDROS_DISK_IMAGE"),
                           ("root", f"f2fs-data0-{arch}.img", "LEANDROS_ROOT_IMAGE")):
        img = clone_path(arch, what)
        try:
            os.unlink(img)
        except OSError:
            pass
        src = os.path.join(REPO, src)
        if subprocess.run([*imgclone.CPC, src, img], capture_output=True).returncode != 0:
            subprocess.run(["cp", src, img], check=True)
        os.environ[var] = img
        if what == "boot":
            subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, cmdline], check=True)
    if drv("start", arch, "--virgl", t=900) != 0:
        drv("stop", t=60)
        sys.exit("boot failed")
    if drv("login", "root", "root", str(int(60 * scale)), t=900) != 0:
        sys.exit("serial login failed")
    ser = driver._Serial()

    def sh(c, t=60):
        return driver.serial_run(c, timeout=t * scale, arch=arch, ser=ser)["output"]

    argv = 'cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" "\\n"'

    def have(name):
        return any(os.path.basename(l.strip()).startswith(name) for l in sh(argv, 90).splitlines())

    def wait_for(name, timeout):
        t = time.time()
        while time.time() - t < timeout * scale:
            if have(name):
                log(name, "up")
                return True
            ser.pump(4)
        log(name, "NOT up")
        return False

    log("cmdline:", sh("cat /proc/cmdline", 20).strip()[-120:])
    wait_for("cosmic-greeter", 150)
    ser.pump(4 * scale)
    typ("leandro")
    key("ret")
    wait_for("cosmic-panel", 150)
    ser.pump(60 * scale)
    snap = ('cat /proc/stat | head -1; for d in /proc/[0-9]*; do echo "P ${d#/proc/} $(cat $d/comm 2>/dev/null) '
            '$(sed "s/.*) //" $d/stat 2>/dev/null | cut -d" " -f12,13) '
            '$(grep -E "ctxt_switches" $d/status 2>/dev/null | cut -d: -f2 | tr -s "\\n\\t " " ")"; done; '
            'for f in events_queued events_coalesced events_overflow instances watches; do echo "I $f $(cat /proc/sys/fs/inotify/$f 2>/dev/null)"; done')
    a = sh(snap, 120)
    t0 = time.time()
    ser.pump(idle * scale)
    b = sh(snap, 120)
    dt = time.time() - t0
    open(f"{out}/a.txt", "w").write(a)
    open(f"{out}/b.txt", "w").write(b)
    log("dt", round(dt, 1))
    open(f"{out}/serial-live.log", "w").write(ser.text() if hasattr(ser, "text") else "")
    if "--keep" not in sys.argv:
        drv("stop", t=60)


if __name__ == "__main__":
    main()
