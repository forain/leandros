#!/usr/bin/env python3
"""An NVMe command timeout must reset the controller, not kill it (L8.4).

A 64 MiB raw image filled with a random pattern is attached as an NVMe disk.
The guest reads 16 MiB of it while QEMU throttles the drive to 4 KiB/s
(`block_set_io_throttle`), so a read command stalls past the driver's 30 s
I/O timeout. The driver must log "controller reset (attempt n/..)", re-create
its queues and re-submit; once the reset is seen the throttle is lifted. The
read must then finish with exactly the pattern, and a later read of another
region must work too. On a driver without reset the controller is marked dead
and both reads fail with EIO.

Usage: LEANDROS_RUN_ID=<lane> python3 nvmefaulttest.py [aarch64|x86_64]
"""
import hashlib, json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MIB = 1024 * 1024


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


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
    img = os.path.join(REPO, f"nvmefault-{run_id}.img")
    data = os.urandom(64 * MIB)
    with open(img, "wb") as f:
        f.write(data)
    want_a = hashlib.sha256(data[:16 * MIB]).hexdigest()
    want_b = hashlib.sha256(data[40 * MIB:48 * MIB]).hexdigest()
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = (
        f"-drive if=none,id=nv,format=raw,file={img} -device nvme,serial=nvfault,drive=nv")
    ok = True
    try:
        drv("stop", timeout=60)
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        _, out = drv("run", "dd if=/dev/nvme0n1 bs=256k skip=160 count=32 iflag=fullblock 2>/dev/null | sha256sum", "60", timeout=120)
        if want_b not in out:
            print("FAIL: baseline read of /dev/nvme0n1 wrong\n" + out.strip()[-300:]); return 1
        print("baseline read ok")
        rc, out = throttle(4096)
        print("throttle on:", out.strip()[-120:])
        drv("run", "sh -c 'dd if=/dev/nvme0n1 of=/tmp/nd.bin bs=256k count=64 iflag=fullblock > /tmp/nd.err 2>&1; "
                   "echo NRC=$? > /tmp/nrc' </dev/null >/dev/null 2>&1 & true", "20")
        t0 = time.time()
        reset = None
        while time.time() - t0 < 90 and not reset:
            time.sleep(3)
            reset = [l for l in serial_log(run_id).splitlines() if "[NVME]" in l and "controller reset (attempt" in l]
        print(f"after {time.time() - t0:.0f} s:", reset[-1] if reset else "no controller reset logged")
        if not reset:
            ok = False
        throttle(0)
        nrc = None
        t0 = time.time()
        while time.time() - t0 < 150:
            _, out = drv("run", "cat /tmp/nrc /tmp/nd.err 2>&1", "20", timeout=60)
            m = re.search(r"NRC=(\d+)", out)
            if m:
                nrc = int(m.group(1)); break
            time.sleep(5)
        print(f"stalled read: rc={nrc}\n" + out.strip()[-200:])
        _, out = drv("run", "sha256sum /tmp/nd.bin", "60", timeout=120)
        if nrc != 0 or want_a not in out:
            ok = False; print("FAIL: stalled read did not return the pattern\n" + out.strip()[-200:])
        else:
            print("stalled read finished after the reset with the pattern intact")
        _, out = drv("run", "dd if=/dev/nvme0n1 bs=256k skip=160 count=32 iflag=fullblock 2>/dev/null | sha256sum; echo ALIVE", "90", timeout=150)
        if want_b not in out:
            ok = False; print("FAIL: I/O after the stall\n" + out.strip()[-300:])
        else:
            print("I/O after the stall ok")
        log = serial_log(run_id)
        for l in [l for l in log.splitlines() if "[NVME]" in l and re.search(r"TIMED OUT|reset|dead", l)][-6:]:
            print(l.strip()[-160:])
        if any("marked dead" in l for l in log.splitlines() if "[NVME]" in l):
            ok = False; print("FAIL: controller marked dead")
    finally:
        try: throttle(0)
        except subprocess.TimeoutExpired: pass
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        pidf = f"/tmp/leandros-{run_id}-qemu.pid"
        try:
            pid = int(open(pidf).read().strip())
            os.kill(pid, 0)
            os.kill(pid, 9); print(f"killed leftover QEMU {pid}")
        except (OSError, ValueError):
            pass
        try: os.unlink(img)
        except OSError: pass
    print("PASS nvmefault" if ok else "FAIL nvmefault")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
