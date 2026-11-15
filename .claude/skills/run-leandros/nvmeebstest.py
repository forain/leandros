#!/usr/bin/env python3
"""Amazon EBS-style NVMe behaviour (EC2 lane L2), tested on QEMU `-device nvme`.

QEMU's controller is 1b36:0010, so the Amazon quirks are forced with the
kernel command line `nvme.quirks=amzn`. One boot, root file system on NVMe
(LEANDROS_ROOTFS_BUS=nvme) without `nvme.rw=1`, plus an extra controller with
serial `vol0123456789abcdef0`, a 4 KiB namespace 1 and a 512 B namespace 2:

  * the root disk is writable although `nvme.rw=1` is absent (Amazon policy);
  * the extra disk stays read-only (only the root disk is auto-writable);
  * `/dev/disk/by-id/nvme-<model>_<serial>` (+ `_2`, `-partN`) opens the
    namespaces, sysfs `device/serial` carries the volume id;
  * a read stalled 45 s (QEMU throttle) is logged ("still pending after 30 s")
    but does NOT reset the controller (I/O timeout 300 s), and completes with
    the right data once the throttle is lifted.

Usage: LEANDROS_RUN_ID=<lane> python3 nvmeebstest.py [aarch64|x86_64]
"""
import hashlib, json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MIB = 1024 * 1024
SERIAL = "vol0123456789abcdef0"
LINK = "/dev/disk/by-id/nvme-QEMU_NVMe_Ctrl_" + SERIAL


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def throttle(bps_rd):
    return drv("qmp", "block_set_io_throttle", json.dumps({
        "device": "nv", "bps": 0, "bps_rd": bps_rd, "bps_wr": 0,
        "iops": 0, "iops_rd": 0, "iops_wr": 0}), timeout=30)


def serial_log(run_id):
    _, log = drv("log", timeout=30)
    try: log += open(f"/tmp/leandros-{run_id}-serial-full.log", errors="replace").read()
    except OSError: pass
    return log


def run(cmd, t="60"):
    return drv("run", cmd, t, timeout=int(t) + 60)[1]


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
    img1 = os.path.join(REPO, f"nvmeebs-{run_id}-1.img")
    img2 = os.path.join(REPO, f"nvmeebs-{run_id}-2.img")
    boot = os.path.join(REPO, f"nvmeebs-{run_id}-boot.img")
    d1, d2 = os.urandom(64 * MIB), os.urandom(8 * MIB)
    open(img1, "wb").write(d1); open(img2, "wb").write(d2)
    h1 = hashlib.sha256(d1[:MIB]).hexdigest()
    h2 = hashlib.sha256(d2[:MIB]).hexdigest()
    hb = hashlib.sha256(d1[40 * MIB:44 * MIB]).hexdigest()
    ha = hashlib.sha256(d1[:4 * MIB]).hexdigest()
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    if subprocess.run(["cp", "-c", src, boot]).returncode != 0:
        import shutil; shutil.copyfile(src, boot)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), boot,
                    "nvme.quirks=amzn"], check=True, stdout=subprocess.DEVNULL)
    env = dict(os.environ)
    env["LEANDROS_DISK_IMAGE"] = boot
    env["LEANDROS_ROOTFS_BUS"] = "nvme"
    env["LEANDROS_QEMU_EXTRA"] = (
        f"-drive if=none,id=nv,format=raw,file={img1} -drive if=none,id=nv2,format=raw,file={img2} "
        f"-device nvme,id=nvc,serial={SERIAL} "
        "-device nvme-ns,bus=nvc,drive=nv,nsid=1,logical_block_size=4096,physical_block_size=4096 "
        "-device nvme-ns,bus=nvc,drive=nv2,nsid=2")
    ok = True

    def check(cond, what, detail=""):
        nonlocal ok
        print(("ok   " if cond else "FAIL ") + what + ("" if cond else "  " + detail.strip()[-200:]))
        ok = ok and bool(cond)

    try:
        drv("stop", timeout=60)
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        log = serial_log(run_id)
        check("Amazon quirks: DMA addresses limited to 48 bits, I/O timeout 300 s" in log, "quirks logged at probe")
        out = run("touch /ebs-rw-check && echo RWOK; cat /sys/class/block/nvme0n1/ro")
        check("RWOK" in out and re.search(r"^0$", out, re.M), "root disk writable without nvme.rw=1", out)
        out = run("cat /sys/class/block/nvme1n1/ro /sys/class/block/nvme1n1/device/serial")
        check(re.search(r"^1$", out, re.M) and SERIAL in out, "extra EBS disk read-only, serial in sysfs", out)
        out = run(f"dd if={LINK} bs=1M count=1 2>/dev/null | sha256sum; "
                  f"dd if={LINK}_2 bs=1M count=1 2>/dev/null | sha256sum")
        check(h1 in out and h2 in out, "by-id link opens ns1 (4 KiB LBA) and ns2 (_2)", out)
        out = run("dd if=/dev/nvme1n1 bs=1M count=1 2>/dev/null | sha256sum")
        check(h1 in out, "direct node reads the same bytes", out)

        throttle(4096)
        run("sh -c 'dd if=/dev/nvme1n1 of=/tmp/nd.bin bs=256k skip=0 count=16 iflag=fullblock > /tmp/nd.err 2>&1; "
            "echo NRC=$? > /tmp/nrc' </dev/null >/dev/null 2>&1 & true", "20")
        t0 = time.time(); warned = False
        while time.time() - t0 < 100 and not warned:
            time.sleep(5)
            warned = any("[NVME]" in l and "still pending after 30 s" in l for l in serial_log(run_id).splitlines())
        check(warned, f"stall warned after {time.time() - t0:.0f} s")
        throttle(0)
        nrc = None; t0 = time.time()
        while time.time() - t0 < 150 and nrc is None:
            out = run("cat /tmp/nrc 2>&1", "20")
            m = re.search(r"NRC=(\d+)", out)
            if m: nrc = int(m.group(1))
            else: time.sleep(5)
        out = run("sha256sum /tmp/nd.bin")
        check(nrc == 0 and ha in out, "stalled read completed with the pattern", out)
        log = serial_log(run_id)
        bad = [l for l in log.splitlines() if "[NVME]" in l and re.search(r"TIMED OUT|controller reset \(attempt|marked dead", l)]
        check(not bad, "no controller reset before the 300 s cap", "\n".join(bad[-3:]))
        out = run("dd if=/dev/nvme1n1 bs=1M skip=40 count=4 2>/dev/null | sha256sum; echo ALIVE", "90")
        check(hb in out, "I/O after the stall", out)
    finally:
        try: throttle(0)
        except subprocess.TimeoutExpired: pass
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        for f in (img1, img2, boot):
            try: os.unlink(f)
            except OSError: pass
    print("PASS nvmeebs" if ok else "FAIL nvmeebs")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
