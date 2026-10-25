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
import hashlib, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MIB = 1024 * 1024
MAX_RTT = 3.0   # an echo through the serial shell normally takes < 1 s


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


STALL_CMDLINE = "usbms.stall_lba=25600 usbms.stall_ms=20000"


def cmdline_image(arch, run_id):
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    img = os.path.join(REPO, f"usbstall-boot-{run_id}.img")
    if os.path.exists(img): os.unlink(img)
    if subprocess.run(["cp", "-c", src, img]).returncode != 0:
        import shutil; shutil.copyfile(src, img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, STALL_CMDLINE],
                   check=True, stdout=subprocess.DEVNULL)
    return img


def serial_log(run_id):
    _, log = drv("log", timeout=30)
    try: log += open(f"/tmp/leandros-{run_id}-serial-full.log", errors="replace").read()
    except OSError: pass
    return log


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
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
        drv("run", "sh -c 'dd if=/dev/sda of=/dev/null bs=64k skip=200 count=4 > /tmp/sd.err 2>&1; "
                   "echo SRC=$? > /tmp/src' </dev/null >/dev/null 2>&1 & true", "20")
        time.sleep(6)   # first transfer deadline passes: the ladder is running
        worst, rtts, t0 = 0.0, [], time.time()
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
            time.sleep(1)
        print(f"shell round trips during the stall: n={len(rtts)} worst={worst:.1f} s "
              f"median={sorted(rtts)[len(rtts) // 2]:.1f} s")
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
        for l in [l for l in lines if "[USB-MSC]" in l][-6:]:
            print(l.strip()[-160:])
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
