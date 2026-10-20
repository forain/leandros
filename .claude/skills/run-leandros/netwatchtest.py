#!/usr/bin/env python3
"""netwatchtest.py [aarch64|x86_64] [--keep] [--only nonic|withnic|loss]
-- the `netwatch=` / `netwatch.loss=` kernel knobs.

Three boots on a copy of the boot image (cmdline `netwatch=20` for the first
two, `netwatch.loss=10` for the third); --only picks one case (default all):

  nonic  LEANDROS_NIC=none (no NIC at all): the console must show
         `[netwatch] no network address after 20 s ... rebooting to the
         fallback entry`, then the VM resets (a second `[netwatch] armed`
         from the next boot) inside the same QEMU process
  withnic  a normal boot with the NIC: no netwatch reboot; /proc/uptime
         passes 40 s and the log has no `no network address` line

  loss   a normal boot, address up (`[NET] DHCP configured`); no reboot for
         ~15 s with the link up; then the QEMU monitor `set_link net0 off`
         drops the carrier, the net daemon drops the address after its 4 s
         grace, and the console must show `[netwatch] network address lost
         for 10 s ... rebooting to the fallback entry` followed by a second
         `[netwatch] armed` in the same QEMU process (the delay from
         set_link to the line is reported, expected ~14 s)

Exit 0 iff all selected cases PASS. Run id `<caller run id>-netwatch`.
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
DRV = os.path.join(HERE, "driver.py")
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))


def main(argv):
    arch, keep, only = "aarch64", False, None
    it = iter(argv)
    for a in it:
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--keep":
            keep = True
        elif a == "--only":
            only = next(it, None)
            if only not in ("nonic", "withnic", "loss"):
                print("--only needs nonic|withnic|loss")
                return 2
    want = lambda c: only is None or only == c  # noqa: E731
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
        imgclone.clone_file(src, img)

    def set_cmdline(cl):
        subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, cl], check=True)

    set_cmdline("netwatch=20")
    env = dict(os.environ, LEANDROS_DISK_IMAGE=img)
    os.environ["LEANDROS_DISK_IMAGE"] = img

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)

    try:
        stop()
        # 1: no NIC
        if want("nonic"):
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
        if want("withnic"):
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
            stop()
        # 3: address loss after carrier loss
        if want("loss"):
            set_cmdline("netwatch.loss=10")
            env.pop("LEANDROS_NIC", None)
            subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
            lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                                env=env, capture_output=True, text=True, timeout=1200)
            pid = driver._qemu_pid()
            t0 = time.time()
            while time.time() - t0 < 120 * scale and "[NET] DHCP configured" not in log():
                time.sleep(2)
            text = log()
            dhcp = "[NET] DHCP configured" in text
            armed0 = text.count("[netwatch] armed")
            time.sleep(15)  # link up: no netwatch reboot may happen
            text = log()
            quiet = "[netwatch] network address lost" not in text and text.count("[netwatch] armed") == armed0
            resp = driver._monitor_send("set_link net0 off")
            t1 = time.time()
            pat = r"\[netwatch\] network address lost for 10 s[^\r\n]*rebooting to the fallback entry"
            line, elapsed = None, 0.0
            while time.time() - t1 < 90 * scale:
                text = log()
                line = re.search(pat, text)
                if line:
                    elapsed = time.time() - t1
                    break
                time.sleep(0.5)
            rebooted = False
            t2 = time.time()
            while time.time() - t2 < 120 * scale:
                if log().count("[netwatch] armed") > armed0:
                    rebooted = True
                    break
                time.sleep(1)
            verdict("loss", lg.returncode == 0 and dhcp and quiet and bool(line) and rebooted
                    and driver._qemu_pid() == pid,
                    f"dhcp={dhcp} quiet_while_up={quiet} "
                    + (f"line after {elapsed:.1f} s from set_link" if line else f"no loss line (monitor: {resp!r})")
                    + ("" if rebooted else " (no reset seen)"))
            stop()
    finally:
        if not keep:
            stop()
        shutil.rmtree(tmp, ignore_errors=True)
    ok = bool(results) and all(results.values())
    print(f"netwatchtest: {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in results.items()))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
