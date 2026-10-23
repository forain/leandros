#!/usr/bin/env python3
"""Dead virtio-blk test (lane/blkfault, 2026-10-19).

Boots with one extra virtio-blk disk behind QEMU's blkdebug driver, then
suspends that disk's next read (`qemu-io <drive> "break read_aio r1"`): the
device never completes the request, exactly like a hung host backend. A
guest read of the disk must fail with EIO after the driver's 30 s timeout,
and the system must keep running. Until 2026-10-19 the driver spun on the
used ring forever, IRQ-masked when the caller was a page fault.

Usage: LEANDROS_RUN_ID=<lane> python3 blkhangtest.py [aarch64|x86_64]
"""
import json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
    img = os.path.join(REPO, f"blkhang-{os.environ.get('LEANDROS_RUN_ID', 'x')}.img")
    with open(img, "wb") as f:
        f.truncate(16 << 20)
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = (
        # blkdebug only sees the read_aio event (and so the break) when a
        # format driver sits above it: raw emits BLKDBG_READ_AIO on its file.
        f"-drive if=none,id=hang,driver=raw,file.driver=blkdebug,"
        f"file.image.driver=file,file.image.filename={img} "
        "-device virtio-blk-pci,drive=hang")
    ok = True
    try:
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        # The 16 MiB disk is the one whose size is 32768 sectors.
        rc, out = drv("run", "cat /proc/partitions; ls /dev | tr '\\n' ' '", "30")
        # 16 MiB = 16384 KiB in /proc/partitions; else the extra disk is the last vdX.
        m = re.search(r"\b16384\s+(vd[a-z]+)\b", out)
        if not m:
            names = sorted(set(re.findall(r"\b(vd[a-z]+)\b", out)), key=lambda n: (len(n), n))
            m = re.match(r"(.*)", names[-1]) if names else None
        if not m:
            print("FAIL: hang disk not found\n" + out[-600:]); return 1
        dev = "/dev/" + m.group(1)
        print(f"hang disk = {dev}"); print(out[:500])
        rc, out = drv("qmp", "human-monitor-command",
                      json.dumps({"command-line": 'qemu-io hang "break read_aio r1"'}))
        print("break armed:", out.strip()[-200:])
        t0 = time.time()
        rc, out = drv("run", f"dd if={dev} of=/dev/null bs=4096 count=1 skip=3001 iflag=direct; echo DDRC=$?", "120",
                      timeout=180)
        secs = time.time() - t0
        print(out.strip()[-400:])
        mrc = re.search(r"DDRC=(\d+)", out)
        if not mrc or mrc.group(1) == "0":
            ok = False; print("FAIL: read of the hung disk did not fail")
        elif secs > 100:
            ok = False; print(f"FAIL: read took {secs:.0f} s")
        else:
            print(f"read failed after {secs:.0f} s as expected")
        # Still alive: a fresh command answers, and the dead disk fails fast.
        t1 = time.time()
        rc, out = drv("run", f"dd if={dev} of=/dev/null bs=4096 count=1 skip=2000; echo DDRC=$?; echo ALIVE", "60")
        if "ALIVE" not in out:
            ok = False; print("FAIL: system unresponsive after the hang\n" + out[-400:])
        elif time.time() - t1 > 20:
            ok = False; print("FAIL: dead disk did not fail fast")
        else:
            print("system alive, dead disk fails fast")
        _, log = drv("log", timeout=30)
        # serial.log is trimmed; the dead line may have rotated out of it.
        try: log += open(f"/tmp/leandros-{os.environ.get('LEANDROS_RUN_ID', 'x')}-serial-full.log", errors="replace").read()
        except OSError: pass
        hit = [l for l in log.splitlines() if "VIRTIO-BLK" in l and "dead" in l]
        print("\n".join(hit[-2:]) or "(no VIRTIO-BLK dead line in log)")
        if not hit: ok = False
        # The wait must take timer ticks (IRQ window), not spin masked.
        wd = [l for l in log.splitlines() if "[WDOG]" in l and "/bin/dd" in l]
        if wd:
            ok = False; print("FAIL: dd spun IRQ-masked\n" + wd[-1])
    finally:
        # A request still suspended in blkdebug blocks QEMU's drain, so
        # quit would hang: resume it first, then kill our QEMU if it stays.
        for args, t in ((("qmp", "human-monitor-command",
                          json.dumps({"command-line": 'qemu-io hang "resume r1"'})), 30),
                        (("stop",), 60)):
            try: drv(*args, timeout=t)
            except subprocess.TimeoutExpired: pass
        pidf = f"/tmp/leandros-{os.environ.get('LEANDROS_RUN_ID', 'x')}-qemu.pid"
        try:
            pid = int(open(pidf).read().strip())
            os.kill(pid, 0)
            os.kill(pid, 9); print(f"killed leftover QEMU {pid}")
        except (OSError, ValueError):
            pass
        try: os.unlink(img)
        except OSError: pass
    print("PASS blkhang" if ok else "FAIL blkhang")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
