#!/usr/bin/env python3
"""a64itstest.py [--keep] -- MSI / MSI-X on aarch64 through the GICv3 ITS (lane a64its).

Two boots of the aarch64 image on QEMU virt (TCG, `gic-version=3,its=on`) with
a virtio-net NIC and an extra NVMe disk:

  boot 1 (ITS on, `nvme.msix=1`):
    its_ready    `[ITS] ready` in the serial log
    net_msix     `[NET] MSI-X armed` (virtio-net RX queue on an LPI)
    nvme_msix    `I/O CQ interrupt vector 0 (MSI-X)` (NVMe CQ on an LPI)
    proc_irq     /proc/interrupts lists >= 2 `ITS-MSI` LPIs (INTID >= 8192)
    net_smoke    ping to the slirp gateway works and the net LPI count rises
    disk_smoke   a read from /dev/nvme0n1 works and the NVMe LPI count rises
  boot 2 (`gic.its=0`):
    its_off      `[ITS] disabled by gic.its=0`, no `[ITS] ready`
    intx_net     `[NET] INTx armed` and the ping still works
Exit 0 iff all PASS. Fail-first: before lane a64its none of the `[ITS]` lines
exist, virtio-net stays on INTx and /proc/interrupts does not exist.
"""
import os
import imgclone
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")


def main(argv):
    keep = "--keep" in argv
    if "x86_64" in argv:
        print("a64itstest: SKIP (aarch64 only)")
        return 0
    arch = "aarch64"
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    tmp = tempfile.mkdtemp(prefix="a64its-")
    nvme = os.path.join(tmp, "nvme.img")
    with open(nvme, "wb") as f:
        f.truncate(64 * 1024 * 1024)
    run_id = f"{caller}-a64its" if caller else "a64its"
    sys.path.insert(0, HERE)
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"a64its_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def boot(tag, cmdline, extra):
        img = os.path.join(tmp, f"boot-{tag}.img")
        root = os.path.join(tmp, f"root-{tag}.img")
        for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                         (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
            if subprocess.run(["cp", "-c", src, dst], capture_output=True).returncode != 0:
                imgclone.clone_file(src, dst)
        r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, cmdline], capture_output=True, text=True)
        if r.returncode != 0:
            print(f"a64itstest: could not set the command line: {r.stdout}{r.stderr}")
            return None
        env = dict(os.environ)
        env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root,
                    "LEANDROS_NET": "user", "LEANDROS_SMP": "4", "LEANDROS_QEMU_EXTRA": extra})
        os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
        sys.modules.pop("driver", None)
        import driver  # noqa: E402
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        r = subprocess.run([sys.executable, DRV, "start", arch, "uefi-tcg"], env=env, capture_output=True, text=True, timeout=900)
        scale = driver.wait_scale(arch, "uefi-tcg")
        if r.returncode != 0:
            print(f"a64itstest: boot failed: {r.stdout[-300:]}{r.stderr[-300:]}")
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        return driver, env, scale, (r.returncode == 0 and lg.returncode == 0)

    def stop(env):
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)

    nvme_extra = ("-machine virt,gic-version=3,its=on "
                  f"-drive if=none,id=nvm0,format=raw,file={nvme} -device nvme,drive=nvm0,serial=a64its")
    try:
        # ── boot 1: ITS on
        b = boot("on", "nvme.msix=1", nvme_extra)
        if b is None:
            return 1
        driver, env, scale, ok = b

        def sh(c, t=60):
            res = driver.serial_run(c, timeout=t * scale, arch=arch)
            return res.get("rc"), res.get("output", "") or ""

        def log():
            try:
                return open(driver.SERIAL_FULL_LOG, errors="replace").read()
            except OSError:
                return ""

        lg = log()
        verdict("its_ready", re.search(r"\[ITS\] ready", lg) is not None)
        verdict("net_msix", re.search(r"\[NET\] MSI-X armed", lg) is not None)
        verdict("nvme_msix", re.search(r"I/O CQ interrupt vector 0 \(MSI-X\)", lg) is not None)

        def lpis():
            _, out = sh("cat /proc/interrupts", 30)
            return [(int(m.group(1)), int(m.group(2))) for m in re.finditer(r"^\s*(\d+):\s+(\d+)\s+ITS-MSI", out, re.M)]

        if ok:
            before = lpis()
            verdict("proc_irq", len(before) >= 2 and all(i >= 8192 for i, _ in before), str(before))
            rc, out = sh("ping -c 4 10.0.2.2 2>&1 | tail -n 2", 60)
            mid = lpis()
            verdict("net_smoke", "0% packet loss" in out and sum(c for _, c in mid) > sum(c for _, c in before),
                    out.strip()[-80:])
            rc, out = sh("dd if=/dev/nvme0n1 of=/dev/null bs=64k count=64 2>&1 | tail -n 1; echo done", 60)
            after = lpis()
            grew = [i for (i, c) in after if c > dict(mid).get(i, 0)]
            verdict("disk_smoke", "done" in out and "rror" not in out and bool(after) and max(i for i, _ in after) in grew, f"{out.strip()[-60:]} grew={grew}")
        else:
            for n in ("proc_irq", "net_smoke", "disk_smoke"):
                verdict(n, False, "no login")
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, f"/tmp/a64its-{arch}-on-serial.log")
        except Exception:  # noqa: BLE001
            pass
        stop(env)

        # ── boot 2: ITS forced off
        b = boot("off", "gic.its=0", nvme_extra)
        if b is None:
            return 1
        driver, env, scale, ok = b
        lg = log()
        verdict("its_off", "[ITS] disabled by gic.its=0" in lg and "[ITS] ready" not in lg)
        _, out = sh("ping -c 2 10.0.2.2 2>&1 | tail -n 1", 60) if ok else (1, "")
        verdict("intx_net", "[NET] INTx armed" in lg and "0% packet loss" in out, out.strip()[-60:])
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, f"/tmp/a64its-{arch}-off-serial.log")
        except Exception:  # noqa: BLE001
            pass
        stop(env)
    finally:
        if not keep:
            shutil.rmtree(tmp, ignore_errors=True)
    ok = bool(results) and all(results.values())
    print(f"a64itstest: {'PASS' if ok else 'FAIL'} {sum(results.values())}/{len(results)} ({arch})", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
