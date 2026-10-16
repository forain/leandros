#!/usr/bin/env python3
"""nmrealtest.py [aarch64|x86_64] [--keep] [--no-reboot] -- the NetworkManager
shim on THE REAL STACK, no mock anywhere, in its own boot.

    nmrs 3.4.0 (nmtest, the applet's library) -> D-Bus -> leandros-sysbus
    networkmanager (/dev/netctl backend) -> /dev/netctl -> wifi_mgr ->
    wifi_sim (wifi0) + virtio-net eth0

Boots throwaway copies of the boot and root images (APFS clones; the
profile slots it writes never touch the build images) with the simulated Wi-Fi's NIC
(LEANDROS_NIC2=wifisim: MAC 02:57:53:00:00:01 on SLIRP 10.0.3.0/24) and eth0 on
SLIRP, logs in as root on the serial console, then:

  mock_off       the shipped image cannot pick the mock backend: no
                 /etc/leandros/nm-mock, no nmshim.mock on the command line,
                 the session service file sets no LEANDROS_NM_* variable
  netdev         leandro is in group netdev (108)
  profiles_boot  netctl published the boot profiles with generation >= 1
  nmtest_real    `nmtest --real` as leandro (initgroups: netdev) under its own
                 dbus-run-session, the shim spawned with the netctl backend:
                 AP list (SAE-only and hidden SSIDs not shown, transition as
                 PSK), wrong key -> AuthFailed + device 120/7, right key ->
                 activated + DHCP on wifi0, profile in a slot, disconnect
                 < 10 s and no autoconnect after it, saved-profile reconnect,
                 open network, refused association, wired admin down/up,
                 radio off/on; ends connected to SimHome with it saved
  nmtest_ro      `nmtest --real --ro` as leandro WITHOUT netdev: sees
                 everything, cannot connect or toggle, Update is an overlay
  profiles_saved `netctl profiles` (root) lists SimHome, autoconnect
  reboot         `reboot`; the kernel loads the slots at boot and wifi_mgr
                 autoconnects to SimHome before any login
  nmtest_reboot  `nmtest --real --after-reboot`: the shim shows it

Run id: `<caller run id>-nmreal` (`nmreal` without one). A running VM of the
caller's run id is stopped first (as wifisimtest does). Exit 0 iff all PASS.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
SVC = "/usr/share/dbus-1/services/org.freedesktop.NetworkManager.service"
AS_LEANDRO = "/bin/nmtest --as leandro"
DRS = "-- /bin/sh /usr/bin/dbus-run-session -- /bin/nmtest --spawn --real"


def running_arch():
    sys.path.insert(0, HERE)
    import driver  # noqa: E402  (caller's LEANDROS_RUN_ID)
    pid = driver._qemu_pid()
    if pid is None:
        return None
    r = subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True)
    for a in ("x86_64", "aarch64"):
        if f"qemu-system-{a}" in r.stdout:
            return a
    return None


def main(argv):
    arch, keep, do_reboot = None, False, True
    for a in argv:
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--keep":
            keep = True
        elif a == "--no-reboot":
            do_reboot = False
        else:
            print(__doc__)
            return 2
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    detected = running_arch()
    arch = arch or detected or "aarch64"
    if detected is not None:
        print(f"nmrealtest: stopping the running VM (run id '{caller}')", flush=True)
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    tmp = tempfile.mkdtemp(prefix="nmreal-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    src = os.path.join(REPO, f"leandros-limine-{arch}.img")
    if subprocess.run(["cp", "-c", src, img], capture_output=True).returncode != 0:
        shutil.copyfile(src, img)
    # The root filesystem too: the test saves a Wi-Fi profile in
    # /etc/leandros/network, which must not reach the build image.
    root = os.path.join(tmp, f"root-{arch}.img")
    rsrc = os.path.join(REPO, f"f2fs-data0-{arch}.img")
    if subprocess.run(["cp", "-c", rsrc, root], capture_output=True).returncode != 0:
        shutil.copyfile(rsrc, root)

    run_id = f"{caller}-nmreal" if caller else "nmreal"
    env = dict(os.environ)
    env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root,
                "LEANDROS_NIC2": "wifisim", "LEANDROS_NET": "user"})
    os.environ.update({"LEANDROS_RUN_ID": run_id})
    sys.modules.pop("driver", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402  (now scoped to the nmreal run id)
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def sh(c, t=60):
        r = driver.serial_run(c, timeout=t * scale, arch=arch)
        return r.get("rc"), r.get("output", "")

    def login():
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        return lg.returncode == 0

    def nmtest(args, name, t=400):
        rc, out = sh(args, t)
        print(out.strip(), flush=True)
        m = re.search(r"NMTEST pass=(\d+) fail=(\d+)", out)
        verdict(name, rc == 0 and m is not None and m.group(2) == "0",
                m.group(0) if m else f"rc={rc}")

    try:
        if driver._qemu_pid() is not None:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        t0 = time.time()
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0 or not login():
            print(f"nmrealtest: boot/login failed: {r.stdout[-400:]}{r.stderr[-400:]}")
            return 1
        print(f"nmrealtest: booted + logged in after {time.time() - t0:.0f} s (arch {arch}, scale {scale})", flush=True)

        # No grep in the guest: read the files, decide here.
        _, cmdline = sh("cat /proc/cmdline")
        _, svc = sh(f"cat {SVC}")
        _, etc = sh("ls -la /etc/leandros")
        bad = [w for w, hit in (("nm-mock file", re.search(r"\snm-mock\s*$", etc, re.M)),
                                ("nmshim.mock cmdline", "nmshim.mock" in cmdline),
                                ("LEANDROS_NM in the service", "LEANDROS_NM" in svc),
                                ("service file missing", "Exec=/usr/libexec/leandros-sysbus networkmanager" not in svc)) if hit]
        verdict("mock_off", not bad, ", ".join(bad) or "no mock switch in the image")

        _, group = sh("cat /etc/group")
        m = re.search(r"^netdev:x:108:(.*)$", group, re.M)
        verdict("netdev", m is not None and "leandro" in m.group(1).strip().split(","), m.group(0).strip() if m else "no netdev group")

        try:
            log = open(driver.SERIAL_FULL_LOG, errors="replace").read()
        except FileNotFoundError:
            log = ""
        m = re.search(r"\[netctl\] profiles gen (\d+): (\d+) loaded \(source (\d+), status (\d+)\)", log)
        verdict("profiles_boot", m is not None and int(m.group(1)) >= 1, m.group(0) if m else "no boot load line")

        rc, out = sh("cat /proc/net/nic; netctl ifaces", 60)
        print(out.strip(), flush=True)

        nmtest(f"{AS_LEANDRO} {DRS}", "nmtest_real", 600)
        nmtest(f"{AS_LEANDRO} --no-netdev {DRS} --ro", "nmtest_ro", 300)

        rc, out = sh("netctl profiles", 60)
        print(out.strip(), flush=True)
        verdict("profiles_saved", rc == 0 and "SimHome" in out, "")

        if do_reboot:
            mark = os.path.getsize(driver.SERIAL_FULL_LOG) if os.path.exists(driver.SERIAL_FULL_LOG) else 0
            sh("sync", 60)
            try:
                driver.serial_run("reboot", timeout=20 * scale, arch=arch)
            except Exception:  # noqa: BLE001  (the shell dies with the system)
                pass
            time.sleep(5 * scale)
            if not login():
                verdict("reboot", False, "no login prompt after reboot")
            else:
                # The kernel autoconnects from the slots before any login.
                auto = None
                t1 = time.time()
                while time.time() - t1 < 90 * scale and auto is None:
                    try:
                        with open(driver.SERIAL_FULL_LOG, "rb") as f:
                            f.seek(mark)
                            text = f.read().decode(errors="replace")
                    except FileNotFoundError:
                        text = ""
                    g = re.search(r"\[netctl\] profiles gen (\d+): (\d+) loaded[^\r\n]*", text)
                    d = re.search(r"DHCP configured, address: (10\.0\.3\.\d+) \(wifi0\)", text)
                    if g and d:
                        auto = (g.group(0), d.group(1))
                    else:
                        time.sleep(2)
                verdict("reboot", auto is not None and int(re.search(r"gen (\d+)", auto[0]).group(1)) >= 1,
                        f"{auto}" if auto else "no wifi0 lease after the reboot")
                nmtest(f"{AS_LEANDRO} {DRS} --after-reboot", "nmtest_reboot", 300)
    finally:
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"nmrealtest: kept: LEANDROS_RUN_ID={run_id} LEANDROS_DISK_IMAGE={img} LEANDROS_ROOT_IMAGE={root}")
    ok = bool(results) and all(results.values())
    print(f"nmrealtest: {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in results.items()))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
