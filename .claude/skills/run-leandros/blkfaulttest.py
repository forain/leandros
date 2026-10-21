#!/usr/bin/env python3
"""Page fault on a dead virtio-blk must raise SIGBUS (lane f2fsfu, L4.11 item 6).

A file on an F2FS volume (a second virtio-blk disk behind QEMU's blkdebug
driver) is mapped, the disk's next read is suspended (`break read_aio`), and
the process then touches the mapped pages. The fault cannot be served: the
driver gives up after its timeout and the process must die of SIGBUS (shell
status 135). Reading zeros or garbage instead (READ_OK / status 0) is data
corruption; no answer at all is a hang.

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
    arch = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
    img = os.path.join(REPO, f"blkfault-{run_id}.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    relabel(img)
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = (
        # blkdebug only sees the read_aio event (and so the break) when a
        # format driver sits above it: raw emits BLKDBG_READ_AIO on its file.
        f"-drive if=none,id=hang,driver=raw,file.driver=blkdebug,"
        f"file.image.driver=file,file.image.filename={img} "
        "-device virtio-blk-pci,drive=hang" + (",disable-legacy=on" if arch == "aarch64" else ""))
    ok = True
    try:
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        rc, out = drv("run",
                      f"mkdir -p /mnt/bf && mount -t f2fs LABEL={LABEL} /mnt/bf && "
                      "dd if=/dev/urandom of=/mnt/bf/f bs=4096 count=256 && sync && umount /mnt/bf && "
                      f"mount -t f2fs LABEL={LABEL} /mnt/bf && echo MOUNT_OK", "120", timeout=200)
        print(out.strip()[-300:])
        if "MOUNT_OK" not in out:
            print("FAIL: could not prepare the volume"); return 1
        drv("run", "setsid sh -c '/bin/f2fstest mmapfault /mnt/bf/f 10 > /tmp/fo 2>&1; "
                   "echo FRC=$? > /tmp/frc' </dev/null >/dev/null 2>&1 &", "20")
        mapped = False
        t0 = time.time()
        while time.time() - t0 < 8:
            _, out = drv("run", "cat /tmp/fo", "10")
            if "MAPPED" in out:
                mapped = True; break
            time.sleep(1)
        if not mapped:
            print("FAIL: mmapfault did not map\n" + out[-300:]); return 1
        rc, out = drv("qmp", "human-monitor-command",
                      json.dumps({"command-line": 'qemu-io hang "break read_aio r1"'}))
        print("break armed:", out.strip()[-200:])
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
        t1 = time.time()
        rc, out = drv("run", "echo ALIVE", "60")
        if "ALIVE" not in out or time.time() - t1 > 30:
            ok = False; print("FAIL: system unresponsive after the fault\n" + out[-300:])
        _, log = drv("log", timeout=30)
        hit = [l for l in log.splitlines() if "VIRTIO-BLK" in l and "dead" in l]
        print("\n".join(hit[-2:]) or "(no VIRTIO-BLK dead line in log)")
        if not hit: ok = False
        wd = [l for l in log.splitlines() if "[WDOG]" in l and "f2fstest" in l]
        if wd:
            ok = False; print("FAIL: f2fstest tripped the watchdog\n" + wd[-1])
    finally:
        for args, t in ((("qmp", "human-monitor-command",
                          json.dumps({"command-line": 'qemu-io hang "resume r1"'})), 30),
                        (("stop",), 60)):
            try: drv(*args, timeout=t)
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
    print("PASS blkfault" if ok else "FAIL blkfault")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
