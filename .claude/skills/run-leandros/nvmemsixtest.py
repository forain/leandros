#!/usr/bin/env python3
"""NVMe over MSI-X (EC2 lane L8), tested on QEMU x86_64 `-device nvme`.

Two boots with the root file system on NVMe (LEANDROS_ROOTFS_BUS=nvme) and the
Amazon quirks forced (`nvme.quirks=amzn`, so the root is writable):

  * `nvme.msix=1`: the probe log names the vector, the first interrupt is
    logged, data read through a second controller matches, and the power-off
    statistics line `MSI-X interrupts since boot: N` shows N above the number
    of I/O commands the workload issued;
  * `nvme.msix=0`: the probe says polled and no interrupt line is printed
    (polled mode is the fallback and must keep working).

Also prints the guest CPU ticks (/proc/stat) of the same read workload in both
modes (informational, not gated). x86_64 only: aarch64 has no MSI-X until the
ITS lane lands.

Usage: LEANDROS_RUN_ID=<lane> python3 nvmemsixtest.py
"""
import hashlib, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MIB = 1024 * 1024
BLOCKS = 2048
# NVMEMSIX_IOPS=<n>: throttle the disk to n IOPS (EBS-like latency) for the CPU comparison.
THROTTLE = f",throttling.iops-total={os.environ['NVMEMSIX_IOPS']}" if os.environ.get("NVMEMSIX_IOPS") else ""
WORKLOAD = f"dd if=/dev/nvme1n1 bs=4k count={BLOCKS} 2>/dev/null | sha256sum"


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def serial_log(run_id):
    _, log = drv("log", timeout=30)
    try: log += open(f"/tmp/leandros-{run_id}-serial-full.log", errors="replace").read()
    except OSError: pass
    return log


def cpu_ticks(text):
    m = re.search(r"cpu\s+([\d ]+)", text)
    if not m: return None
    v = [int(x) for x in m.group(1).split()]
    return sum(v) - v[3] - (v[4] if len(v) > 4 else 0)   # all but idle / iowait


def one_mode(arch, run_id, mode, img, want_hash, base, check):
    boot_img = os.path.join(REPO, f"nvmemsix-{run_id}-{mode}-boot.img")
    if subprocess.run(["cp", "-c", base, boot_img]).returncode != 0:
        import shutil; shutil.copyfile(base, boot_img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), boot_img,
                    f"nvme.quirks=amzn nvme.msix={mode}"], check=True, stdout=subprocess.DEVNULL)
    env = dict(os.environ)
    env["LEANDROS_DISK_IMAGE"] = boot_img
    env["LEANDROS_ROOTFS_BUS"] = "nvme"
    env["LEANDROS_QEMU_EXTRA"] = (
        f"-drive if=none,id=nv,format=raw,file={img}{THROTTLE} -device nvme,id=nvc,serial=msixtest "
        "-device nvme-ns,bus=nvc,drive=nv,nsid=1")
    cpu = None
    try:
        drv("stop", timeout=60)
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            check(False, f"msix={mode}: boot", out); return None
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            check(False, f"msix={mode}: login", out); return None
        log = serial_log(run_id)
        if mode == "1":
            check(re.search(r"\[NVME\] nvme\d+: MSI-X vector 0 -> IDT vector", log), "msix=1: probe names the vector")
            check("MSI-X) in" in log, "msix=1: I/O queue created in MSI-X mode")
        else:
            check("MSI-X vector 0" not in log and ", polled) in" in log, "msix=0: polled")
        t0 = drv("run", "head -1 /proc/stat", "30", timeout=90)[1]
        out = drv("run", WORKLOAD, "120", timeout=200)[1]
        check(want_hash in out, f"msix={mode}: data through the controller is correct", out)
        t1 = drv("run", "head -1 /proc/stat", "30", timeout=90)[1]
        a, b = cpu_ticks(t0), cpu_ticks(t1)
        if a is not None and b is not None: cpu = b - a
        drv("run", "poweroff", "10", timeout=60)
        time.sleep(8)
        log = serial_log(run_id)
        m = re.findall(r"nvme\d+: MSI-X interrupts since boot: (\d+)", log)
        if mode == "1":
            check(m and max(int(x) for x in m) >= BLOCKS // 64, "msix=1: interrupt counter rose", str(m))
            check("first MSI-X interrupt" in log, "msix=1: first interrupt logged")
        else:
            check(not m and "first MSI-X interrupt" not in log, "msix=0: no interrupts counted", str(m))
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        try: os.unlink(boot_img)
        except OSError: pass
    return cpu


def main():
    arch = "x86_64"
    if os.environ.get("LEANDROS_TEST_ARCH") == "aarch64":
        print("SKIP nvmemsix: x86_64-only test (own QEMU x86_64 guest); aarch64 has no MSI-X yet")
        return 0
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
    img = os.path.join(REPO, f"nvmemsix-{run_id}.img")
    data = os.urandom(8 * MIB)
    open(img, "wb").write(data)
    want = hashlib.sha256(data[:BLOCKS * 4096]).hexdigest()
    base = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    ok = True

    def check(cond, what, detail=""):
        nonlocal ok
        print(("ok   " if cond else "FAIL ") + what + ("" if cond else "  " + str(detail).strip()[-200:]))
        ok = ok and bool(cond)

    try:
        c1 = one_mode(arch, run_id, "1", img, want, base, check)
        c0 = one_mode(arch, run_id, "0", img, want, base, check)
        print(f"info: workload CPU ticks msix=1 {c1} vs polled {c0}")
    finally:
        try: os.unlink(img)
        except OSError: pass
    print("PASS nvmemsix" if ok else "FAIL nvmemsix")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
