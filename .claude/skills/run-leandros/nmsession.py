#!/usr/bin/env python3
"""nmsession.py <arch> <outdir> [--gpu virgl|venus] [--keep] [--real] [--no-netdev]

The COSMIC network applet against the NetworkManager shim, for screenshots:

  boot --virgl, serial root login, `touch /etc/leandros/nm-mock` (the shim
  then simulates eth0 + wifi0 + APs), greeter login as leandro, wait for
  cosmic-panel and cosmic-applet-network, screenshot.

  --real       no mock: the shim's /dev/netctl backend on the kernel's real
               stack, with the simulated Wi-Fi as wifi0 (LEANDROS_NIC2=wifisim,
               eth0 on SLIRP; wifi_sim's APs SimHome, SimCafe, SimMixed ...)
  --no-netdev  take leandro out of group netdev (/etc/group) before the
               greeter login: the session is read-only (see, not change)

The VM boots a throwaway APFS clone of the boot image
(/tmp/leandros-<run id>-nmsession-<arch>.img, removed by `stop`), so
nm-mock, /etc/group edits and saved Wi-Fi profiles never reach the build
image.

With --keep QEMU stays up afterwards for `nmsession.py click X Y SHOT` and
`nmsession.py key KEYS... ` / `nmsession.py type TEXT` / `nmsession.py shot
SHOT` steps (coordinates are framebuffer pixels of the last screenshot), and
`nmsession.py stop`. All use LEANDROS_RUN_ID=nmshim (VNC 5941), so another
lane's QEMU is never touched.

Output in <outdir>: desktop.png, serial-live.log, steps.json.
"""
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "nmshim")
os.environ.setdefault("LEANDROS_VNC_PORT", "5941")
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


def clone_path(arch):
    return f"/tmp/leandros-{os.environ['LEANDROS_RUN_ID']}-nmsession-{arch}.img"


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
    if sub == "stop":
        drv("stop", t=60)
        for a in ("aarch64", "x86_64"):
            try:
                os.unlink(clone_path(a))
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
    img = clone_path(arch)
    try:
        os.unlink(img)
    except OSError:
        pass
    src = os.path.join(REPO, f"leandros-limine-{arch}.img")
    if subprocess.run(["cp", "-c", src, img], capture_output=True).returncode != 0:
        subprocess.run(["cp", src, img], check=True)
    os.environ["LEANDROS_DISK_IMAGE"] = img
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

    if "--real" not in A:
        log(sh("touch /etc/leandros/nm-mock && ls -l /etc/leandros/nm-mock"))
    else:
        log(sh("ls -la /etc/leandros /etc/leandros/network; cat /proc/net/wifi"))
    if "--no-netdev" in A:
        # No sed in the guest: rewrite /etc/group from here, netdev emptied.
        lines = [l.rstrip("\r") for l in sh("cat /etc/group").splitlines() if re.match(r"^[a-z_][^:]*:", l)]
        lines = [("netdev:x:108:" if l.startswith("netdev:") else l) for l in lines]
        sh("printf '%s\\n' " + " ".join(f"'{l}'" for l in lines) + " > /etc/group")
        log(sh("cat /etc/group"))
    log(sh("ls -l /bin/cosmic-applet-network /usr/libexec/leandros-sysbus "
           "/usr/share/applications/com.system76.CosmicAppletNetwork.desktop "
           "/usr/share/dbus-1/services/org.freedesktop.NetworkManager.service"))

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
    wait_for("applet", lambda: have("cosmic-applet-network"), 90)
    # /proc/<pid>/cmdline shows only argv[0] here, so the shim cannot be told
    # apart from the other sysbus services by its argument; its mock creates
    # its profile directory at start instead.
    wait_for("shim", lambda: "leandros-nm-mock" in sh("ls /tmp", 30) or "--real" in A, 60)
    ser.pump(6 * scale)
    shot(f"{out}/desktop.ppm")
    log("The Network applet is the rightmost panel icon (a wired-port symbol with eth0 up).")
    json.dump(steps, open(f"{out}/steps.json", "w"), indent=1)
    open(f"{out}/serial-live.log", "w").write(ser.text() if hasattr(ser, "text") else "")
    if "--keep" not in A:
        drv("stop", t=60)


if __name__ == "__main__":
    main()
