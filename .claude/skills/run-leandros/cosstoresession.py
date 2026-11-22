#!/usr/bin/env python3
"""btsession.py <arch> <outdir> [--gpu virgl|venus] [--keep] [--no-bluetooth]

The COSMIC Bluetooth applet against the org.bluez shim (leandros-sysbus bluez)
on a scripted controller: boot --virgl, serial root login, start
`bttest serve /run/bluetooth/btd.sock /tmp/bt.keys` (btd on the scripted
controller with "LeandrOS Test Pad" 06:05:04:03:02:01 behind it; QEMU has no
HCI, so the boot-time btd exited), greeter login as leandro, wait for
cosmic-panel and cosmic-applet-bluetooth, then print the session's
leandros-sysbus[bluez] log lines (found through /proc/<pid>/fd/2).

  --no-bluetooth  take leandro out of group bluetooth (/etc/group) before the
                  login: the shim must fail to reach btd (EACCES) and the
                  applet shows no adapter.

Same throwaway-clone, --keep, click/key/type/shot/stop and `run CMD` (root
serial command) conventions as nmsession.py; LEANDROS_RUN_ID=btsession.
"""
import imgclone
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "btsession")
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
    return f"/tmp/leandros-{os.environ['LEANDROS_RUN_ID']}-btsession-{what}-{arch}.img"


def main():
    sub = sys.argv[1]
    if sub == "click":
        x, y, out = int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
        ref = sys.argv[5] if len(sys.argv) > 5 else None
        w, h = ppm_size(ref) if ref else (1280, 800)
        click(x, y, w, h)
        time.sleep(float(os.environ.get("NMS_SETTLE", "3")))
        shot(out)
        return
    if sub == "key":
        for k in sys.argv[2:]:
            key(k)
        return
    if sub == "type":
        typ(sys.argv[2])
        return
    if sub == "shot":
        shot(sys.argv[2])
        return
    if sub == "run":
        ser = driver._Serial()
        print(driver.serial_run(sys.argv[2], timeout=float(os.environ.get("BTS_T", "60")), arch=os.environ.get("BTS_ARCH", "aarch64"), ser=ser)["output"])
        return
    if sub == "stop":
        drv("stop", t=60)
        for a in ("aarch64", "x86_64"):
            for w in ("boot", "root"):
                try:
                    os.unlink(clone_path(a, w))
                except OSError:
                    pass
        return

    arch, out = sys.argv[1], sys.argv[2]
    A = sys.argv[3:]
    gpu = A[A.index("--gpu") + 1] if "--gpu" in A else "virgl"
    os.makedirs(out, exist_ok=True)
    scale = driver.wait_scale(arch)
    steps = {}
    log(f"nmsession arch={arch} gpu={gpu} scale={scale}")
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
    if "--real" in A:
        os.environ.setdefault("LEANDROS_NIC2", "wifisim")
        os.environ.setdefault("LEANDROS_NET", "user")
    if drv("start", arch, f"--{gpu}", t=900) != 0:
        drv("stop", t=60)
        sys.exit("boot failed")
    if drv("login", "root", "root", str(int(60 * scale)), t=900) != 0:
        sys.exit("serial login failed")
    ser = driver._Serial()

    def sh(c, t=60):
        r = driver.serial_run(c, timeout=t * scale, arch=arch, ser=ser)
        return r["output"]

    argv = 'cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" "\\n"'

    def have(name):
        return any(os.path.basename(l.strip()).startswith(name) for l in sh(argv, 90).splitlines())

    def wait_for(name, pred, timeout):
        t = time.time()
        while time.time() - t < timeout * scale:
            if pred():
                steps[name] = round(time.time() - t, 1)
                log(f"{name}: after {steps[name]} s")
                return True
            ser.pump(4)
        steps[name] = None
        log(f"{name}: NOT ready after {timeout * scale:.0f} s")
        return False

    wait_for("greeter", lambda: have("cosmic-greeter"), 150)
    ser.pump(4 * scale)
    typ("leandro")
    key("ret")
    wait_for("panel", lambda: have("cosmic-panel"), 150)
    ser.pump(10 * scale)
    pp = sh("pgrep -x cosmic-panel | head -1").split()[-1] if False else None
    log(sh("ls -l /bin/cosmic-store /usr/share/applications/com.system76.CosmicStore.desktop", 30))
    log(sh("P=$(for d in /proc/[0-9]*; do [ \"$(cat $d/comm 2>/dev/null)\" = cosmic-panel ] && echo ${d#/proc/}; done | head -1); echo panel=$P; "
           "tr '\\0' '\\n' < /proc/$P/environ | grep -E '^(XDG_RUNTIME_DIR|HOME|USER|DBUS_SESSION|XDG_SESSION|LANG)' > /tmp/env.txt; cat /tmp/env.txt; ls -l $(grep XDG_RUNTIME /tmp/env.txt | cut -d= -f2)", 60))
    log(sh("set -a; . /tmp/env.txt 2>/dev/null; set +a; export WAYLAND_DISPLAY=wayland-1; unset WAYLAND_SOCKET; "
           "(cosmic-store > /tmp/store.log 2>&1 & echo $! > /tmp/store.pid); true", 30))
    ser.pump(20 * scale)
    log(sh("P=$(cat /tmp/store.pid); echo pid=$P; ls -d /proc/$P && cat /proc/$P/status | head -3; wc -l /tmp/store.log; grep -ciE 'panic|neither' /tmp/store.log; tail -n 25 /tmp/store.log", 40))
    json.dump(steps, open(f"{out}/steps.json", "w"), indent=1)
    open(f"{out}/serial-live.log", "w").write(ser.text() if hasattr(ser, "text") else "")
    if "--keep" not in A:
        drv("stop", t=60)


if __name__ == "__main__":
    main()
