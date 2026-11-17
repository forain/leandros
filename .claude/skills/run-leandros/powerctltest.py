#!/usr/bin/env python3
"""powerctltest.py <arch> -- power off / reboot as an unprivileged user, the
way a desktop does it (lane powerctl).

  1. ssh as leandro (not a local session): `reboot` and `systemctl poweroff`
     are refused (init: DENIED) and the VM stays up.
  2. greeter user (cosmic-greeter, uid 990), on the greeter's own bus
     (/run/user/990/bus, which busd/greeter-launch now provide): login1
     CanReboot answers "yes" and Reboot reboots the VM (init: "reboot
     requested ... authorised", then a fresh boot).
  3. greeter login as leandro, then on leandro's session bus: CanPowerOff is
     "yes"; `/bin/reboot` (non-root + a bus => login1) reboots the VM.
  4. greeter login again; login1 PowerOff over D-Bus powers the VM off
     (QEMU exits).

The D-Bus clients are started from the root serial shell with sudo -u; what
init authorises is the login1 service itself (a child of the greeter's or the
session's busd, i.e. of greetd), exactly the process the greeter's buttons and
cosmic-osd reach. Boots its own image clones; LEANDROS_RUN_ID must be set.
Prints `powerctl_<step>: PASS|FAIL` and a summary; exit 0 iff all passed.
"""
import os
import hostclean
import re
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402
import imgclone  # noqa: E402
import nmsession as nms  # noqa: E402  (key/typ/drv/clone_path helpers)
import sshsmoke  # noqa: E402

RES = []


def step(name, ok, detail=""):
    RES.append((name, bool(ok)))
    print(f"powerctl_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)
    return ok


def log_size():
    try:
        return os.path.getsize(driver.SERIAL_FULL_LOG)
    except OSError:
        return 0


def log_since(mark):
    try:
        with open(driver.SERIAL_FULL_LOG, "rb") as f:
            f.seek(mark)
            return f.read().decode(errors="replace")
    except OSError:
        return ""


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    scale = driver.wait_scale(arch)
    REPO = nms.REPO
    for what, src, var in (("boot", f"leandros-limine-{arch}.img", "LEANDROS_DISK_IMAGE"),
                           ("root", f"f2fs-data0-{arch}.img", "LEANDROS_ROOT_IMAGE")):
        img = nms.clone_path(arch, what)
        if os.path.exists(img):
            os.unlink(img)
        imgclone.clone_file(os.path.join(REPO, src), img)
        os.environ[var] = img
    try:
        if nms.drv("start", arch, "--virgl", t=900) != 0:
            return step("boot", False, "boot failed")
        run(arch, scale)
    finally:
        nms.drv("stop", t=60)
        for w in ("boot", "root"):
            try:
                os.unlink(nms.clone_path(arch, w))
            except OSError:
                pass
    ok = bool(RES) and all(v for _, v in RES)
    print(f"powerctltest: {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in RES))
    return 0 if ok else 1


def run(arch, scale):
    def root_login():
        return nms.drv("login", "root", "root", str(int(120 * scale)), t=1200) == 0

    if not step("login1", root_login()):
        return
    ser = driver._Serial()

    def sh(c, t=60):
        return driver.serial_run(c, timeout=t * scale, arch=arch, ser=ser)["output"]

    argv = 'cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" "\\n"'

    def wait_proc(name, timeout):
        t = time.time()
        while time.time() - t < timeout * scale:
            if any(os.path.basename(l.strip()).startswith(name) for l in sh(argv, 90).splitlines()):
                return True
            ser.pump(4)
        return False

    uid = re.search(r"^(\d+)\s*$", sh("/bin/id -u leandro") or "", re.M)
    uid = uid.group(1) if uid else "1000"

    # 1. a non-session caller (ssh) is refused
    host, port = sshsmoke.target()
    tmp = tempfile.mkdtemp(prefix="powerctl-")
    hostclean.track(tmp)
    ssh = sshsmoke.Ssh(host, port, tmp)
    mark = log_size()
    rc, out, err = ssh.run("leandro", "/bin/reboot; echo rc=$?; /bin/systemctl poweroff; echo rc=$?",
                           password="leandro", timeout=90)
    time.sleep(3)
    text = log_since(mark)
    denied = len(re.findall(r"requested by pid \d+ \(uid \d+\): DENIED", text))
    step("ssh_refused", denied >= 2 and "Access denied" in err and driver._qemu_pid() is not None
         and "going down" not in text, f"denied={denied} rc_lines={out.count('rc=1')} err={err.strip()[-120:]!r}")

    # 2. greeter user: CanReboot + Reboot on the greeter's bus
    if not step("greeter_up", wait_proc("cosmic-greeter", 180)):
        return
    gbus = "DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/user/990/bus"
    gsys = f"/bin/sudo -u cosmic-greeter /usr/bin/env {gbus} /usr/libexec/leandros-sysbus power"
    can = sh(f"{gsys} CanReboot", 60)
    step("greeter_can_reboot", re.search(r"^yes\s*$", can, re.M), can.strip()[-160:])
    mark = log_size()
    try:
        sh(f"{gsys} Reboot", 30)
    except Exception:  # noqa: BLE001
        pass
    t = time.time()
    while time.time() - t < 60 * scale and "System is going down" not in log_since(mark):
        time.sleep(1)
    text = log_since(mark)
    m = re.search(r"reboot requested by pid \d+ \(uid (\d+)\): (\w+)", text)
    step("greeter_reboot", m and m.group(1) == "990" and m.group(2) == "authorised"
         and "System is going down" in text, m.group(0) if m else text.strip()[-200:])
    ser.close()  # one serial client at a time: `driver.py login` needs the socket
    if not step("boot2", root_login()):
        return
    ser = driver._Serial()

    def session_login():
        if not wait_proc("cosmic-greeter", 180):
            return False
        ser.pump(4 * scale)
        nms.typ("leandro")
        nms.key("ret")
        return wait_proc("cosmic-panel", 180)

    ubus = f"DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"
    usys = f"/bin/sudo -u leandro /usr/bin/env {ubus} /usr/libexec/leandros-sysbus power"

    # 3. leandro's session: CanPowerOff, then `reboot` through login1
    if not step("session2", session_login()):
        return
    can = sh(f"{usys} CanPowerOff", 60)
    step("user_can_poweroff", re.search(r"^yes\s*$", can, re.M), can.strip()[-160:])
    mark = log_size()
    try:
        sh(f"/bin/sudo -u leandro /usr/bin/env {ubus} /bin/reboot", 30)
    except Exception:  # noqa: BLE001
        pass
    t = time.time()
    while time.time() - t < 60 * scale and "System is going down" not in log_since(mark):
        time.sleep(1)
    text = log_since(mark)
    m = re.search(r"reboot requested by pid \d+ \(uid (\d+)\): (\w+)", text)
    step("user_reboot_cmd", m and m.group(1) == uid and m.group(2) == "authorised"
         and "System is going down" in text, m.group(0) if m else text.strip()[-200:])
    ser.close()
    if not step("boot3", root_login()):
        return
    ser = driver._Serial()

    # 4. leandro's session: PowerOff over D-Bus, QEMU exits
    if not step("session3", session_login()):
        return
    mark = log_size()
    try:
        sh(f"{usys} PowerOff", 30)
    except Exception:  # noqa: BLE001
        pass
    t = time.time()
    while time.time() - t < 90 * scale and driver._qemu_pid() is not None:
        time.sleep(1)
    text = log_since(mark)
    m = re.search(r"power-?off requested by pid \d+ \(uid (\d+)\): (\w+)", text)
    step("user_poweroff_dbus", m and m.group(1) == uid and m.group(2) == "authorised"
         and driver._qemu_pid() is None, (m.group(0) if m else text.strip()[-200:])
         + ("" if driver._qemu_pid() is None else " QEMU still running"))


if __name__ == "__main__":
    sys.exit(hostclean.run(main))
