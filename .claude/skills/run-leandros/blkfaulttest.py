#!/usr/bin/env python3
"""Page fault on a dead virtio-blk must raise SIGBUS (lane f2fsfu, L4.11 item 6).

A file on an F2FS volume (a second virtio-blk disk behind QEMU's blkdebug
driver) is mapped. The blkdebug config then switches to a state in which every
read_aio completes with EIO (a write after MAPPED triggers the switch), and the
process touches the mapped pages. The fault cannot be served: the device
reports an I/O error and the process must die of SIGBUS (shell status 135).
Reading zeros or garbage instead (READ_OK / status 0) is data corruption; no
answer at all is a hang.

A hung device (`break read_aio`) is deliberately NOT used: since commit
80fc911e the driver never abandons a slow request while the device is alive.

Usage: LEANDROS_RUN_ID=<lane> python3 blkfaulttest.py [aarch64|x86_64]
"""
import json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
LABEL = "f2fsbf-test"


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def relabel(img):
    name = LABEL.encode("utf-16-le")
    with open(img, "r+b") as f:
        for base in (0, 4096):
            f.seek(base + 1024 + 124)
            f.write(name + bytes(1024 - len(name)))


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
    img = os.path.join(REPO, f"blkfault-{run_id}.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    relabel(img)
    cfg = os.path.join(REPO, f"blkfault-{run_id}.blkdebug")
    with open(cfg, "w") as f:
        f.write('[set-state]\nstate = "1"\nevent = "write_aio"\nnew_state = "2"\n\n'
                '[inject-error]\nevent = "read_aio"\nstate = "2"\nerrno = "5"\n'
                'immediately = "off"\nonce = "off"\n')
    dev = "-device virtio-blk-pci,drive=hang" + (",disable-legacy=on" if arch == "aarch64" else "")
    penv = dict(os.environ)
    # Phase 1 (plain disk): populate the volume. The error injection must not
    # exist yet, or the setup writes would trip the state change.
    penv["LEANDROS_QEMU_EXTRA"] = f"-drive if=none,id=hang,driver=raw,file={img} " + dev
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = (
        # blkdebug only sees the read_aio event (and so the break) when a
        # format driver sits above it: raw emits BLKDBG_READ_AIO on its file.
        f"-drive if=none,id=hang,driver=raw,file.driver=blkdebug,"
        f"file.config={cfg},file.image.driver=file,file.image.filename={img} "
        + dev)
    ok = True
    try:
        drv("stop", timeout=60)  # runtests may leave its own VM running
        rc, out = drv("start", arch, timeout=300, env=penv)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=penv)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        rc, out = drv("run",
                      f"mkdir -p /mnt/bf && mount -t f2fs LABEL={LABEL} /mnt/bf && "
                      "dd if=/dev/urandom of=/mnt/bf/f bs=4096 count=256 && dd if=/dev/urandom of=/mnt/bf/g bs=4096 count=64 && sync && umount /mnt/bf && echo PREP_OK", "120", timeout=200)
        print(out.strip()[-300:])
        if "PREP_OK" not in out:
            print("FAIL: could not prepare the volume"); return 1
        drv("stop", timeout=60)
        # Phase 2: same disk behind blkdebug, state 1 (reads fine) until the trigger write.
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot 2\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login 2\n" + out[-800:]); return 1
        rc, out = drv("run", f"mkdir -p /mnt/bf && mount -t f2fs LABEL={LABEL} /mnt/bf && echo MOUNT_OK", "120", timeout=200)
        print(out.strip()[-200:])
        if "MOUNT_OK" not in out:
            print("FAIL: could not mount the volume"); return 1
        drv("run", "sh -c '/bin/f2fstest mmapfault /mnt/bf/f 10 > /tmp/fo 2>&1; "
                   "echo FRC=$? > /tmp/frc' </dev/null >/dev/null 2>&1 & true", "20")
        mapped = False
        t0 = time.time()
        while time.time() - t0 < 8:
            _, out = drv("run", "cat /tmp/fo", "10")
            if "MAPPED" in out:
                mapped = True; break
            time.sleep(1)
        if not mapped:
            print("FAIL: mmapfault did not map\n" + out[-300:]); return 1
        # A write after MAPPED moves blkdebug to state 2: reads now fail with EIO.
        _, out = drv("run", "dd if=/dev/zero of=/mnt/bf/trigger bs=4096 count=8 conv=fsync 2>&1; echo TRIGRC=$?", "60")
        print("error injection armed:", out.strip()[-60:])
        if "TRIGRC=0" not in out:
            print("FAIL: trigger write failed"); return 1
        frc = None
        t0 = time.time()
        while time.time() - t0 < 100:
            time.sleep(5)
            _, out = drv("run", "cat /tmp/frc 2>&1; cat /tmp/fo", "20", timeout=60)
            m = re.search(r"FRC=(\d+)", out)
            if m:
                frc = int(m.group(1)); break
        print(f"after {time.time() - t0:.0f} s: frc={frc}\n" + out.strip()[-200:])
        if frc is None:
            ok = False; print("FAIL: hang (the faulting process never finished)")
        elif frc == 135:
            print("process died of SIGBUS as expected")
        elif frc == 0 or "READ_OK" in out:
            ok = False; print("FAIL: silent read (zeros or garbage mapped)")
        else:
            ok = False; print(f"FAIL: unexpected status {frc}")
        # read() of a file never touched since the mount, with the device returning errors:
        # must fail with EIO, not return zeros / a short clean EOF.
        _, out = drv("run", "dd if=/mnt/bf/g of=/dev/null bs=4096 count=64 2>&1; echo DDRC=$?", "90", timeout=150)
        m = re.search(r"DDRC=(\d+)", out)
        if not m or m.group(1) == "0":
            ok = False; print("FAIL: read() with the device returning errors did not fail\n" + out.strip()[-200:])
        elif not re.search(r"(?i)input/output|i/o error|EIO", out):
            ok = False; print("FAIL: read() failed but not with EIO\n" + out.strip()[-200:])
        else:
            print("read() failed with EIO as expected")
        t1 = time.time()
        rc, out = drv("run", "echo ALIVE", "60")
        if "ALIVE" not in out or time.time() - t1 > 30:
            ok = False; print("FAIL: system unresponsive after the fault\n" + out[-300:])
        _, log = drv("log", timeout=30)
        # serial.log is trimmed; lines may have rotated out of it.
        try: log += open(f"/tmp/leandros-{run_id}-serial-full.log", errors="replace").read()
        except OSError: pass
        # An I/O error completion is not a dead device: the disk must stay usable.
        dead = [l for l in log.splitlines() if "VIRTIO-BLK" in l and "marked dead" in l]
        if dead:
            ok = False; print("FAIL: an I/O error completion marked the disk dead\n" + dead[-1])
        wd = [l for l in log.splitlines() if "[WDOG]" in l and "f2fstest" in l]
        if wd:
            ok = False; print("FAIL: f2fstest tripped the watchdog\n" + wd[-1])
    finally:
        for args, t in ((("stop",), 60),):
            try: drv(*args, timeout=t)
            except subprocess.TimeoutExpired: pass
        pidf = f"/tmp/leandros-{run_id}-qemu.pid"
        try:
            pid = int(open(pidf).read().strip())
            os.kill(pid, 0)
            os.kill(pid, 9); print(f"killed leftover QEMU {pid}")
        except (OSError, ValueError):
            pass
        for p in (img, cfg):
            try: os.unlink(p)
            except OSError: pass
    print("PASS blkfault" if ok else "FAIL blkfault")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
