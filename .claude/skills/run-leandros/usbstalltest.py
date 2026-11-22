#!/usr/bin/env python3
"""A stalled USB stick must not hold a CPU through its retry ladder (L8.2).

A 32 MiB raw image is attached as a second USB stick (qemu-xhci +
usb-storage) to a ONE-CPU guest whose root stays on virtio. The kernel is
booted with the in-guest fault knob `usbms.stall_lba=25600 usbms.stall_ms=20000`:
the first request covering sector 25600 opens a 20 s window in which every
bulk transfer waits out its deadline and returns Timeout, so the BOT/SCSI
error ladder (retries, reset recovery, back-off) runs in the guest. (An
earlier version throttled the QEMU drive instead; that is invalid: QEMU
cancels the throttled request synchronously and freezes the vCPU itself.)
During the window the shell must stay responsive (`echo` round trips timed);
afterwards the stalled read must succeed and the disk must still work.

Also reported: the knob's `[USB-MSC] fault:` line (atomic=true would mean the
request reached the stick from a context that may not sleep) and any
`[WDOG] ... soft lockup` line.

NOTE: the read is issued by a sleepable task (read syscall), which parks in
the poll loop on any kernel, so this is a smoke test of the ladder and its
recovery, not a red/green guard for the atomic-context case.

Usage: LEANDROS_RUN_ID=<lane> python3 usbstalltest.py [aarch64|x86_64]
"""
import imgclone
import hashlib, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MIB = 1024 * 1024
MAX_RTT = 3.0   # an echo through the serial shell normally takes < 1 s (KVM/HVF)
MAX_RTT_TCG = 8.0   # TCG with several VMs per host adds scheduling noise (3.5 s seen); a real hold is ~20 s


def rtt_bound(arch):
    sys.path.insert(0, HERE)
    import driver
    return MAX_RTT_TCG if driver.accel_kind(arch) == "tcg" else MAX_RTT


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


STALL_CMDLINE = "usbms.stall_lba=25600 usbms.stall_ms=20000"
if os.environ.get("USBSTALL_DUMP") == "1":   # opt-in: task dumps from the stalled reader (perturbs timing)
    STALL_CMDLINE += " usbms.stall_dump=1"
# Echo median in the stall window may be at most this factor of the no-stall
# baseline median (plus BASE_SLACK s of harness noise): the stall must not
# slow the shell down, only a real hold would.
MEDIAN_FACTOR = 4.0
BASE_SLACK = 1.0


def timerstat():
    """`/proc/stat` timerstat line: ticks, timer irqs, early deadline irqs, uart drains."""
    _, out = drv("run", "grep ^timerstat /proc/stat", "20", timeout=60)
    m = re.search(r"timerstat (\d+) (\d+) (\d+) (\d+)", out)
    return tuple(int(x) for x in m.groups()) if m else None


def echo_rtts(n, pause=1.0, limit=None):
    rtts = []
    for _ in range(n):
        t = time.time()
        try:
            rc, out = drv("run", "echo PONG", "30", timeout=60)
        except subprocess.TimeoutExpired:
            out = ""
        dt = time.time() - t
        if "PONG" not in out:
            dt = max(dt, 30.0)
        rtts.append(dt)
        time.sleep(pause)
    return rtts


def counters_line(tag, a, b, secs):
    if not a or not b:
        return f"timerstat {tag}: unavailable"
    d = [y - x for x, y in zip(a, b)]
    return (f"timerstat {tag}: {secs:.1f} s ticks={d[0]} timer_irqs={d[1]} ({d[1] / secs:.0f}/s) "
            f"deadline_early={d[2]} uart_drains={d[3]} ({d[3] / secs:.0f}/s)")


def cmdline_image(arch, run_id):
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    img = os.path.join(REPO, f"usbstall-boot-{run_id}.img")
    if os.path.exists(img): os.unlink(img)
    if subprocess.run([*imgclone.CPC, src, img]).returncode != 0:
        import shutil; shutil.copyfile(src, img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, STALL_CMDLINE],
                   check=True, stdout=subprocess.DEVNULL)
    return img


def serial_log(run_id):
    _, log = drv("log", timeout=30)
    try: log += open(f"/tmp/leandros-{run_id}-serial-full.log", errors="replace").read()
    except OSError: pass
    return log


# Captured 2026-11-17 (lane usbstall6): the ladder's timestamped kernel line
# glued to a fresh prompt. driver.py must still see the prompt under it, or
# every echo that meets such a line waits out a prompt timeout (3 s / 2x10 s).
_GLUED = (b'PONG\n\n<<LRC:cfe2869e:0>>\n\x1b[?2004h\x1b[0m\x1b[?25l\x1b[66;1H\x1b[J\x1b[39mbrush-0.5# '
          b'\x1b[38;5;14m\x1b[0m\x1b7\x1b8\x1b[?25h[   26.096999] [USB-MSC] lun 0 cmd 0x28: attempt 2 failed: '
          b'Transport(Timeout) (10056 ms into the command)\n[   26.024708] [pcache] 5 s: hits 1158\n')


def main():
    sys.path.insert(0, HERE)
    import driver
    if not driver._at_prompt(_GLUED):
        print("FAIL: driver.py does not see a prompt followed by timestamped kernel lines"); return 1
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
    global MAX_RTT
    MAX_RTT = rtt_bound(arch)
    print(f"usbstalltest: echo RTT bound {MAX_RTT:.1f} s ({arch})", flush=True)
    img = os.path.join(REPO, f"usbstall-{run_id}.img")
    data = os.urandom(32 * MIB)
    with open(img, "wb") as f:
        f.write(data)
    want = hashlib.sha256(data[8 * MIB:12 * MIB]).hexdigest()
    env = dict(os.environ)
    env["LEANDROS_SMP"] = "1"
    env["LEANDROS_QEMU_EXTRA"] = (
        "-device qemu-xhci,id=xhs "
        f"-drive if=none,id=st,format=raw,file={img} -device usb-storage,bus=xhs.0,drive=st,id=usbst")
    ok = True
    boot_img = cmdline_image(arch, run_id)
    env["LEANDROS_DISK_IMAGE"] = boot_img
    try:
        drv("stop", timeout=60)
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "240", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        _, out = drv("run", "ls /dev/sd* 2>&1; dd if=/dev/sda bs=256k skip=32 count=16 iflag=fullblock 2>/dev/null | sha256sum", "60", timeout=120)
        if want not in out:
            print("FAIL: baseline read of /dev/sda wrong\n" + out.strip()[-300:]); return 1
        print("baseline read ok")
        st0 = timerstat(); tb = time.time()
        base = echo_rtts(8)
        st1 = timerstat(); te = time.time()
        base_med = sorted(base)[len(base) // 2]
        print(f"baseline echo (no stall): n={len(base)} worst={max(base):.1f} s median={base_med:.2f} s")
        print(counters_line("baseline", st0, st1, te - tb), flush=True)
        drv("run", "sh -c 'dd if=/dev/sda of=/dev/null bs=64k skip=200 count=4 > /tmp/sd.err 2>&1; "
                   "echo SRC=$? > /tmp/src' </dev/null >/dev/null 2>&1 & true", "20")
        time.sleep(6)   # first transfer deadline passes: the ladder is running
        worst, rtts, t0 = 0.0, [], time.time()
        st2 = timerstat()
        while time.time() - t0 < 35:
            t = time.time()
            try:
                rc, out = drv("run", "echo PONG", "30", timeout=60)
            except subprocess.TimeoutExpired:
                out = ""
            dt = time.time() - t
            if "PONG" not in out:
                dt = max(dt, 30.0)
            rtts.append(dt); worst = max(worst, dt)
            if dt > 2.0:
                print(f"slow echo #{len(rtts)}: {dt:.1f} s, started {t - t0:.1f} s into the window", flush=True)
            time.sleep(1)
        med = sorted(rtts)[len(rtts) // 2]
        print(f"shell round trips during the stall: n={len(rtts)} worst={worst:.1f} s "
              f"median={med:.2f} s (baseline median {base_med:.2f} s)")
        st3 = timerstat()
        print(counters_line("stall window", st2, st3, time.time() - t0), flush=True)
        if med > MEDIAN_FACTOR * base_med + BASE_SLACK:
            ok = False; print(f"FAIL: stall-window echo median {med:.2f} s > {MEDIAN_FACTOR}x baseline {base_med:.2f} s + {BASE_SLACK} s")
        if worst > MAX_RTT:
            ok = False; print(f"FAIL: the CPU was held (an echo took {worst:.1f} s > {MAX_RTT} s)")
        src = None
        t0 = time.time()
        while time.time() - t0 < 150:
            _, out = drv("run", "cat /tmp/src /tmp/sd.err 2>&1", "20", timeout=60)
            m = re.search(r"SRC=(\d+)", out)
            if m:
                src = int(m.group(1)); break
            time.sleep(5)
        print(f"stalled read finished: rc={src}")
        if src != 0:
            ok = False; print("FAIL: stalled read did not succeed after the window")
        _, out = drv("run", "dd if=/dev/sda bs=256k skip=32 count=16 iflag=fullblock 2>/dev/null | sha256sum", "90", timeout=150)
        if want not in out:
            ok = False; print("FAIL: I/O after the stall\n" + out.strip()[-300:])
        else:
            print("I/O after the stall ok")
        log = serial_log(run_id)
        lines = log.splitlines()
        msc = [l for l in lines if "[USB-MSC]" in l or "[WDOG]" in l]
        if not ok:   # task dumps taken from the stalled reader (usbms.stall_dump=1)
            for l in [l for l in lines if "[TASKS]" in l][:120]:
                print(l.strip()[-200:])
        for l in (msc if not ok else msc[-6:]):   # a failure keeps the whole ladder + any atomic/lockup note
            print(l.strip()[-200:])
        for l in [l for l in lines if "soft lockup" in l][-2:]:
            print(l.strip()[-160:])
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        for f in (img, boot_img):
            try: os.unlink(f)
            except OSError: pass
    print("PASS usbstall" if ok else "FAIL usbstall")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
