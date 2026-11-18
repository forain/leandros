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

# dd's exit status, read back between unique brackets. The serial stream also
# carries kernel diagnostic lines that are not synchronised with the shell's
# output, and the kernel drops UART bytes under host back-pressure (bounded
# putc wait, main.rs UART_LINE_OPEN): a "[FORK] ... holds=1" line whose head
# was dropped leaves a bare "1" line. The old check took ANY digits-only line
# for dd's status, so such a fragment failed the run with "read of the hung
# disk completed without the backend" while dd was still blocked (box aarch64
# full regress, lane inoid 2026-11-17).
RC_CMD = 'echo "DDRC<$(cat /tmp/dd.rc 2>/dev/null)>"'


def dd_rc(out):
    """dd's exit status from RC_CMD's output: digits, "" (still running) or
    None (marker not seen)."""
    m = re.search(r"DDRC<(\d*)>", out)
    return m.group(1) if m else None


def selftest():
    # Captured shape: a kernel line whose prefix was dropped, then the probe.
    spliced = "[    5.24] [FORK] tgid=24 clone_us=996 holds=\n1\nDDRC<>\n"
    old = bool(re.search(r"^\d+\s*$", spliced.replace("DDRC<>", "").strip(), re.M))
    new = dd_rc(spliced)
    print(f"selftest: old parser says completed={old}, new parser rc={new!r}")
    ok = new == "" and dd_rc("x\nDDRC<0>\n") == "0" and dd_rc("noise 7\n") is None
    print("PASS selftest" if ok else "FAIL selftest")
    return 0 if ok else 1


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
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
    ok = selftest() == 0  # the parser must see through spliced kernel fragments
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
        # Since 2026-10-25 (80fc911e) a slow request is never abandoned: like
        # Linux virtio-blk the driver warns at 30 s and keeps waiting. So the
        # hung read must stay pending, the system must stay responsive, the
        # driver must log the slow request, and the read must complete once
        # the backend resumes.
        sfull = f"/tmp/leandros-{os.environ.get('LEANDROS_RUN_ID', 'x')}-serial-full.log"
        def slowlines():
            try: txt = open(sfull, errors="replace").read()
            except OSError: return []
            return [l for l in txt.splitlines() if "VIRTIO-BLK" in l and "slow request" in l and "still waiting" in l]
        rc, out = drv("run", f"rm -f /tmp/dd.rc; (dd if={dev} of=/dev/null bs=4096 count=1 skip=3001 iflag=direct; echo $? > /tmp/dd.rc) >/tmp/dd.out 2>&1 </dev/null & sleep 2; echo STARTED", "30")
        if "STARTED" not in out:
            ok = False; print("FAIL: could not start background read\n" + out[-300:])
        rc, out = drv("run", RC_CMD, "30")
        st = dd_rc(out)
        if st is None:
            ok = False; print("FAIL: system unresponsive during the hang\n" + out[-300:])
        elif st != "":
            ok = False; print("FAIL: read of the hung disk completed without the backend\n" + out[-300:])
        else:
            print("system alive while the disk is hung")
        t0 = time.time()
        while time.time() - t0 < 75 and not slowlines():
            time.sleep(3)
        sl = slowlines()
        print("\n".join(sl[-3:]) or "(no slow request line)")
        print("t_slow=%.1fs" % (time.time() - t0))
        _, o = drv("run", "cat /tmp/dd.out; cat /tmp/dd.rc", "30"); print("dd.out:", o.strip()[-300:])
        if not sl:
            ok = False; print("FAIL: driver never reported the slow request")
        rc, out = drv("run", "echo ALIVE2", "30")
        if "ALIVE2" not in out:
            ok = False; print("FAIL: system unresponsive after the slow warning\n" + out[-300:])
        drv("qmp", "human-monitor-command", json.dumps({"command-line": 'qemu-io hang "resume r1"'}), timeout=30)
        got = ""
        for _ in range(10):
            rc, out = drv("run", RC_CMD, "30")
            if dd_rc(out): got = dd_rc(out); break
            time.sleep(2)
        if got != "0":
            ok = False; print(f"FAIL: read did not complete after resume (rc={got!r})")
        else:
            print("read completed after the backend resumed")
        wd = []
        _, log = drv("log", timeout=30)
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
    sys.exit(selftest() if "--selftest" in sys.argv else main())
