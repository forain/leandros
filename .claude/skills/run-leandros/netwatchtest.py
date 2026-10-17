#!/usr/bin/env python3
"""netwatchtest.py [aarch64|x86_64] [--keep] -- the `netwatch=` kernel knob.

Two boots on a copy of the boot image whose cmdline is `netwatch=20`:

  noni   LEANDROS_NIC=none (no NIC at all): the console must show
         `[netwatch] no network address after 20 s ... rebooting to the
         fallback entry`, then the VM resets (a second `[netwatch] armed`
         from the next boot) inside the same QEMU process
  withnic  a normal boot with the NIC: no netwatch reboot; /proc/uptime
         passes 40 s and the log has no `no network address` line

Exit 0 iff both PASS. Run id `<caller run id>-netwatch`.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))


def main(argv):
    arch, keep = "aarch64", False
    for a in argv:
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--keep":
            keep = True
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    run_id = f"{caller}-netwatch" if caller else "netwatch"
    os.environ["LEANDROS_RUN_ID"] = run_id
    os.environ.pop("LEANDROS_NO_REBOOT", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def log():
        try:
            with open(driver.SERIAL_FULL_LOG, "rb") as f:
                return f.read().decode(errors="replace")
        except FileNotFoundError:
            return ""

    tmp = tempfile.mkdtemp(prefix="netwatch-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    subprocess.run(["cp", "-c", src, img], check=False)
    if not os.path.exists(img):
        shutil.copy(src, img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, "netwatch=20"], check=True)
    env = dict(os.environ, LEANDROS_DISK_IMAGE=img)
    os.environ["LEANDROS_DISK_IMAGE"] = img

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)

    try:
        stop()
        # 1: no NIC
        env["LEANDROS_NIC"] = "none"
        r0 = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r0.returncode != 0:
            print("start failed:", (r0.stdout + r0.stderr)[-600:])
        pid = driver._qemu_pid()
        t0 = time.time()
        text = ""
        while time.time() - t0 < 240 * scale:
            text = log()
            m = re.search(r"\[netwatch\] no network address after 20 s[^\r\n]*rebooting to the fallback entry", text)
            if m and text.count("[netwatch] armed") >= 2:
                break
            time.sleep(2)
        line = re.search(r"\[netwatch\] no network address after 20 s[^\r\n]*", text)
        rebooted = text.count("[netwatch] armed") >= 2
        verdict("nonic", bool(line) and rebooted and driver._qemu_pid() == pid,
                (line.group(0).strip() if line else "no netwatch line")
                + ("" if rebooted else " (no reset seen)"))
        stop()
        # 2: with a NIC
        env.pop("LEANDROS_NIC", None)
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                            env=env, capture_output=True, text=True, timeout=1200)
        up = 0
        t0 = time.time()
        while time.time() - t0 < 200 * scale:
            rr = driver.serial_run("cat /proc/uptime", timeout=30 * scale, arch=arch)
            m = re.search(r"^(\d+)\.", rr.get("output", ""), re.M)
            up = int(m.group(1)) if m else 0
            if up > 45:
                break
            time.sleep(5)
        text = log()
        dhcp = "[NET] DHCP configured" in text
        verdict("withnic", lg.returncode == 0 and up > 40 and "[netwatch] no network" not in text and dhcp,
                f"uptime {up} s, dhcp={dhcp}, armed={text.count('[netwatch] armed')}")
    finally:
        if not keep:
            stop()
        shutil.rmtree(tmp, ignore_errors=True)
    ok = bool(results) and all(results.values())
    print(f"netwatchtest: {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in results.items()))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
