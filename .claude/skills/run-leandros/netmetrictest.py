#!/usr/bin/env python3
"""netmetrictest.py [aarch64|x86_64] [--keep] -- `netctl metric` and the
`net.metric.<ifname>=` boot knob (lane rtmetric), in its own boot with eth0
(SLIRP) plus the simulated Wi-Fi (wifi0, NetA via the legacy wifi.ssid=).

Steps (`netmetric_<step>: PASS|FAIL detail`):
  boot_knob   cmdline net.metric.eth0=700: /proc/net/route lists wifi0's
              default (600) before eth0's (700)
  flip        `netctl metric eth0 50`: eth0's default route comes first again
  restore     `netctl metric eth0 default`: back to the boot-knob value 700
  ssh_onlink  eth0's on-link route (10.0.2.0/24) is still present and the
              eth0 sshd banner answers while wifi0 carries the default route
Exit 0 iff all PASS.
"""
import os
import imgclone
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
CMDLINE = ("wifisim.aps=NetA:psk:neta-pass-123:-50:6 wifi.ssid=NetA wifi.psk=neta-pass-123 "
           "net.metric.eth0=700")


def main(argv):
    arch = next((a for a in argv if a in ("aarch64", "x86_64")), None)
    keep = "--keep" in argv
    arch = arch or os.environ.get("LEANDROS_TEST_ARCH") or "aarch64"
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    tmp = tempfile.mkdtemp(prefix="netmetric-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    root = os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        if subprocess.run([*imgclone.CPC, src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)
    r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, CMDLINE], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"netmetrictest: could not set the command line: {r.stdout}{r.stderr}")
        return 1
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p1 = s.getsockname()[1]; s.close()
    run_id = f"{caller}-netmetric" if caller else "netmetric"
    env = dict(os.environ)
    env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root,
                "LEANDROS_NET": "user", "LEANDROS_NIC2": "wifisim", "LEANDROS_NIC2_SSH_PORT": str(p1)})
    os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"netmetric_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def sh(c, t=60):
        res = driver.serial_run(c, timeout=t * scale, arch=arch)
        return res.get("rc"), res.get("output", "") or ""

    def defaults():
        _, out = sh("cat /proc/net/route", 30)
        return [l.split()[0] for l in out.splitlines() if len(l.split()) > 7 and l.split()[1] == "00000000"]

    def routes():
        _, out = sh("cat /proc/net/route", 30)
        return out

    try:
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"netmetrictest: boot failed: {r.stdout[-400:]}{r.stderr[-400:]}")
            return 1
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode != 0:
            print("netmetrictest: login failed")
            return 1
        d = []
        t0 = time.time()
        while time.time() - t0 < 90 * scale:
            d = defaults()
            if "wifi0" in d and "eth0" in d:
                break
            time.sleep(2)
        verdict("boot_knob", d[:2] == ["wifi0", "eth0"], f"defaults {d}")
        sh("netctl metric eth0 50", 30)
        d = defaults()
        _, st = sh("netctl status", 30)
        verdict("flip", d[:1] == ["eth0"] and re.search(r"eth0.*metric 50", st) is not None, f"defaults {d}")
        sh("netctl metric eth0 default", 30)
        d = defaults()
        _, st = sh("netctl status", 30)
        verdict("restore", d[:1] == ["wifi0"] and re.search(r"eth0.*metric 700", st) is not None, f"defaults {d}")
        rt = routes()
        b = ""
        try:
            with socket.create_connection(("127.0.0.1", p1), timeout=10) as so:
                so.settimeout(10)
                b = so.recv(64).decode(errors="replace").strip()
        except OSError:
            pass
        verdict("ssh_onlink", re.search(r"^eth0\s+0002000A\s+00000000", rt, re.M) is not None and defaults()[:1] == ["wifi0"],
                f"onlink eth0 present, default via {defaults()[:1]}")
    finally:
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, f"/tmp/netmetric-{arch}-serial.log")
        except Exception:  # noqa: BLE001
            pass
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
    ok = bool(results) and all(results.values())
    print(f"netmetrictest: {'PASS' if ok else 'FAIL'} {sum(results.values())}/{len(results)} ({arch})", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
