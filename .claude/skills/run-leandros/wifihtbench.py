#!/usr/bin/env python3
"""wifihtbench.py [aarch64|x86_64] [--mb N] [--runs K] [--case NAME=CMDLINE ...]
-- what the RX Block Ack reorder buffer is worth, on the simulated Wi-Fi
(lane wifiht).

Each case is its own boot (a throwaway copy of the boot and root images,
LEANDROS_NIC2=wifisim on SLIRP 10.0.3.0/24, the legacy network SimHome from
the command line). The simulated HT AP sends the downlink as A-MPDU-like
bursts with holes (subframes retransmitted in the next burst), duplicates
and losses (wifisim.ampdu=<hold>:<dup>:<loss> per mille); the station's
reorder buffer (wifi_ht, what iwlwifi runs) puts them back in order unless
wifisim.reorder=0. The measurement: N MB from the host into the guest over
wifi0 (ssh `cat > /dev/null` through the wifi0 port forward), K runs; then
the `ht:` / `rx_ba:` lines of /proc/net/nic.

Default cases:
  legacy     wifisim.ht=0                       (no QoS / HT: frames straight)
  clean      wifisim.ampdu=0:0:0                (HT, BA session, no perturbation)
  reorder    wifisim.ampdu=150:20:2             (holes, dups, losses; reorder on)
  noreorder  wifisim.ampdu=150:20:2 wifisim.reorder=0
Prints one `wifihtbench <case>: <MB/s> ...` line per case.
"""
import os
import imgclone
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
from netroamtest import free_port, running_arch  # noqa: E402
from sshsmoke import Ssh  # noqa: E402

BASE = "wifi.ssid=SimHome wifi.psk=simhome-pass"
CASES = [
    ("legacy", "wifisim.ht=0"),
    ("clean", "wifisim.ampdu=0:0:0"),
    ("reorder", "wifisim.ampdu=150:20:2"),
    ("noreorder", "wifisim.ampdu=150:20:2 wifisim.reorder=0"),
]


def one_case(arch, name, extra, mb, runs, caller):
    tmp = tempfile.mkdtemp(prefix=f"wifiht-{name}-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    root = os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        if subprocess.run([*imgclone.CPC, src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)
    r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, f"{BASE} {extra}"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None, f"could not set the command line: {r.stdout}{r.stderr}"
    port = free_port()
    run_id = f"{caller}-wifiht" if caller else "wifiht"
    env = dict(os.environ)
    env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root, "LEANDROS_NET": "user",
                "LEANDROS_NIC2": "wifisim", "LEANDROS_NIC2_SSH_PORT": str(port)})
    os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    import driver  # noqa: E402  (scoped to this run id)
    scale = driver.wait_scale(arch, "uefi")
    try:
        if driver._qemu_pid() is not None:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            return None, f"boot failed: {r.stdout[-300:]}{r.stderr[-300:]}"
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode != 0:
            return None, "login failed"

        def sh(c, t=60):
            res = driver.serial_run(c, timeout=t * scale, arch=arch)
            return res.get("rc"), res.get("output", "") or ""

        # wifi0 up with a lease.
        t0 = time.time()
        addr = None
        while time.time() - t0 < 90 * scale and addr is None:
            _, out = sh("cat /proc/net/nic", 30)
            sec = out.split("[wifi0]", 1)
            if len(sec) == 2:
                m = re.search(r"^address: (10\.0\.3\.\d+)/\d+", sec[1].split("\n[", 1)[0], re.M)
                addr = m.group(1) if m else None
            if addr is None:
                time.sleep(2)
        if addr is None:
            return None, "wifi0 got no address"
        ssh = Ssh("127.0.0.1", port, tmp)
        deadline = time.time() + 60 * scale
        while time.time() < deadline:
            rk = subprocess.run(["ssh-keyscan", "-T", "5", "-p", str(port), "127.0.0.1"], capture_output=True, timeout=30)
            if b"ssh-" in rk.stdout:
                break
            time.sleep(2)
        payload = os.urandom(1 << 20) * mb
        rates = []
        for _ in range(runs):
            t1 = time.time()
            rc, out, err = ssh.run("leandro", "cat | wc -c", password="leandro", stdin=payload, timeout=int(600 * scale))
            dt = time.time() - t1
            if rc != 0 or out.strip() != str(len(payload)):
                return None, f"transfer failed: rc {rc} out {out.strip()!r} err {err.strip()[-200:]!r}"
            rates.append(len(payload) / dt / 1e6)
        _, nic = sh("cat /proc/net/nic", 30)
        sec = nic.split("[wifi0]", 1)[1].split("\n[", 1)[0] if "[wifi0]" in nic else nic
        ht = "\n    ".join(ln.strip() for ln in sec.splitlines() if ln.startswith(("ht:", "rx_ba:")))
        return rates, ht
    finally:
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, f"/tmp/wifiht-{arch}-{name}-serial.log")
        except Exception:  # noqa: BLE001
            pass
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        shutil.rmtree(tmp, ignore_errors=True)


def main(argv):
    arch, mb, runs, cases = None, 8, 3, []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--mb":
            mb = int(argv[i + 1]); i += 1
        elif a == "--runs":
            runs = int(argv[i + 1]); i += 1
        elif a == "--case":
            n, _, c = argv[i + 1].partition("="); cases.append((n, c)); i += 1
        else:
            print(__doc__)
            return 2
        i += 1
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    detected = running_arch()
    arch = arch or os.environ.get("LEANDROS_TEST_ARCH") or detected or "aarch64"
    if detected is not None:
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
    ok = True
    for name, extra in cases or CASES:
        rates, info = one_case(arch, name, extra, mb, runs, caller)
        if rates is None:
            ok = False
            print(f"wifihtbench {name}: FAIL {info}", flush=True)
            continue
        rs = " ".join(f"{r:.2f}" for r in rates)
        print(f"wifihtbench {name}: {sum(rates) / len(rates):.2f} MB/s ({mb} MB x {len(rates)}: {rs}) [{extra}]\n    {info}", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
