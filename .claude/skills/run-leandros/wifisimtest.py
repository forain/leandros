#!/usr/bin/env python3
"""wifisimtest.py [aarch64|x86_64] [--level N] [--keep] -- the simulated
Wi-Fi (kernel `wifi_sim`) end to end in its own boot.

Boots a throwaway copy of the boot image whose kernel command line says
`wifisim.selftest=<N>` (default 2), with a second virtio-net NIC on its own
SLIRP subnet (10.0.3.0/24, MAC 02:57:53:00:00:01, which turns the sim on and
is its data path). The kernel self-test then drives the Wi-Fi manager through
the netctl seam -- scan (security bits per AP, coalescing, hidden SSID),
wrong passphrase (FAILED wrong-key after the AP's message-1 retries),
right passphrase, DHCP DISCOVER/OFFER over the data path, scan while
associated, DISCONNECT under 2 s, failure reasons (SAE-only, no AP,
association refused), beacon loss, deauth, BSS aging, radio off/on,
autoconnect by priority / blocked after DISCONNECT / skipping a wrong-key
profile / after link loss, DISCONNECT preempting an attempt -- and prints
`[wifisim] selftest: PASS n/n`. This script waits for that verdict, then
logs in and checks /proc/net/wifi lists the simulated APs.

It needs the VM slot: a running VM of the same LEANDROS_RUN_ID is stopped
first (the root image is shared), and the test VM runs as run id
`<caller run id>-wifisim` (`wifisim` without one). As the last `regress` step that is the regress VM, whose logs
runtests has already got. The arch defaults to that of the running VM, else
aarch64.

Exit 0 iff the self-test passed and /proc/net/wifi shows the sim.
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


def running_arch():
    """Arch of the VM of the caller's run id, None when none runs."""
    sys.path.insert(0, HERE)
    import driver  # noqa: E402  (caller's LEANDROS_RUN_ID)
    pid = driver._qemu_pid()
    if pid is None:
        return None
    r = subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True)
    if "qemu-system-x86_64" in r.stdout:
        return "x86_64"
    if "qemu-system-aarch64" in r.stdout:
        return "aarch64"
    return None


def main(argv):
    arch, level, keep = None, 2, False
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--level":
            level = int(argv[i + 1]); i += 1
        elif a == "--keep":
            keep = True
        else:
            print(__doc__)
            return 2
        i += 1
    caller_run_id = os.environ.get("LEANDROS_RUN_ID", "")
    detected = running_arch()
    arch = arch or os.environ.get("LEANDROS_TEST_ARCH") or detected or "aarch64"
    if detected is not None:
        print(f"wifisimtest: stopping the running VM (run id '{caller_run_id}') for the test boot", flush=True)
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    tmp = tempfile.mkdtemp(prefix="wifisim-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    src = os.path.join(REPO, f"leandros-limine-{arch}.img")
    # APFS clone when possible (instant), plain copy otherwise.
    if subprocess.run(["cp", "-c", src, img], capture_output=True).returncode != 0:
        imgclone.clone_file(src, img)
    r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, f"wifisim.selftest={level}"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        print(f"wifisimtest: could not set the command line: {r.stdout}{r.stderr}")
        return 1

    env = dict(os.environ)
    sim_id = f"{caller_run_id}-wifisim" if caller_run_id else "wifisim"
    env["LEANDROS_RUN_ID"] = sim_id
    env["LEANDROS_DISK_IMAGE"] = img
    # The second NIC on 10.0.3.0/24 with the sim's MAC (wifi0's data path).
    env["LEANDROS_NIC2"] = "wifisim"
    os.environ.update({"LEANDROS_RUN_ID": sim_id})
    sys.modules.pop("driver", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402  (now scoped to run id wifisim)
    scale = driver.wait_scale(arch, "uefi")

    ok = False
    proc_ok = False
    try:
        if driver._qemu_pid() is not None:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        t0 = time.time()
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"wifisimtest: boot failed: {r.stdout[-500:]}{r.stderr[-500:]}")
            return 1
        verdict = None
        printed = 0
        deadline = time.time() + 240 * scale
        rx = re.compile(r"\[wifisim\] selftest: (PASS \d+/\d+|FAIL .*)")
        while time.time() < deadline and verdict is None:
            try:
                with open(driver.SERIAL_FULL_LOG, errors="replace") as f:
                    text = f.read()
            except FileNotFoundError:
                text = ""
            mine = [ln for ln in text.splitlines() if "[wifisim]" in ln]
            for ln in mine[printed:]:
                print(ln.strip(), flush=True)
            printed = len(mine)
            m = rx.search(text)
            if m:
                verdict = m.group(1)
            else:
                time.sleep(1)
        print(f"wifisimtest: verdict {verdict!r} after {time.time() - t0:.0f} s (arch {arch}, scale {scale})", flush=True)
        ok = verdict is not None and verdict.startswith("PASS")
        # Userland view: /proc/net/wifi lists the simulated APs.
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(60 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode == 0:
            res = driver.serial_run("cat /proc/net/wifi", timeout=60 * scale, arch=arch)
            out = res.get("output", "")
            print(out.strip())
            proc_ok = res.get("rc") == 0 and "SimHome" in out and "wifi_sim" in out
            print(f"proc_net_wifi: {'PASS' if proc_ok else 'FAIL'}")
            # The self-test leaves wifi0 associated (autoconnect, SimHome):
            # the net server's DHCP must bind 10.0.3.x on it, and traffic to
            # the 10.0.3.2 gateway must leave through it.
            lease = None
            t1 = time.time()
            while time.time() - t1 < 40 * scale and lease is None:
                res = driver.serial_run("cat /proc/net/nic", timeout=30 * scale, arch=arch)
                sec = res.get("output", "").split("[wifi0]", 1)
                if len(sec) == 2:
                    m = re.search(r"^address: (10\.0\.3\.\d+)/\d+", sec[1].split("\n[", 1)[0], re.M)
                    if m:
                        lease = m.group(1)
                if lease is None:
                    time.sleep(2)
            print(f"wifi0_dhcp: {'PASS ' + lease if lease else 'FAIL no 10.0.3.x lease on wifi0'}")
            res = driver.serial_run("ping -c 2 10.0.3.2", timeout=40 * scale, arch=arch)
            ping_ok = res.get("rc") == 0
            print(f"wifi0_ping: {'PASS' if ping_ok else 'FAIL'} {res.get('output', '').strip()[-200:]}")
            proc_ok = proc_ok and lease is not None and ping_ok
            # L10.13: `connect SSID --bssid MAC` must resolve the saved profile
            # (security + PSK) exactly like `connect --uuid`; BSSID only pins the AP.
            res = driver.serial_run("netctl save SimHome simhome-pass --no-auto && netctl disconnect && "
                                    "netctl connect SimHome --bssid 0a:57:53:a0:00:01", timeout=90 * scale, arch=arch)
            out = res.get("output", "")
            bssid_ok = res.get("rc") == 0 and "connected" in out.lower()
            print(f"netctl_connect_ssid_bssid: {'PASS' if bssid_ok else 'FAIL'} {out.strip()[-300:]}")
            proc_ok = proc_ok and bssid_ok
        else:
            print("wifisimtest: login failed")
    finally:
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
    print(f"wifisim_selftest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok and proc_ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
