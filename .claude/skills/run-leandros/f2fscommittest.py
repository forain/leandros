#!/usr/bin/env python3
"""f2fscommittest.py [aarch64|x86_64] [--cmdline "k=v ..."]

A checkpoint commit with an unknown outcome must not let roll-forward fsyncs
append to the older pack (lane f2fscommit). Fresh 64 MiB volume as an extra
disk (run id `<caller>-f2fscm`). Boot 1 carries `f2fs.cpunk=1`: the first
commit after the mount lands on the medium but is reported failed, as after
a write/flush timeout. `f2fstest commitunk` then syncs (that commit), creates
a file and fsyncs it (the claim); QEMU is SIGKILLed right after the claim.
Boot 2 mounts and `f2fstest commitscan` must find the claimed file whole.
Before the fix the fsync appended a roll-forward record to the old pack
while the newer pack (the commit that "failed") won at the next mount: the
acknowledged file was gone. Exit 0 iff the claim survived.
"""
import os
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
LABEL = "f2fscm-test"

caller = os.environ.get("LEANDROS_RUN_ID", "")
os.environ["LEANDROS_RUN_ID"] = f"{caller}-f2fscm" if caller else "f2fscm"
os.environ.pop("LEANDROS_USB_STICK", None)
sys.path.insert(0, HERE)
import driver  # noqa: E402


def stop_caller_vm(caller):
    """Under `runtests host:` the caller's VM holds the root image write lock."""
    if caller:
        subprocess.run([sys.executable, os.path.join(HERE, "driver.py"), "stop"],
                       env=dict(os.environ, LEANDROS_RUN_ID=caller), capture_output=True, timeout=120)


def relabel(img):
    name = LABEL.encode("utf-16-le")
    with open(img, "r+b") as f:
        for base in (0, 4096):
            f.seek(base + 1024 + 124)
            f.write(name + bytes(1024 - len(name)))


def main(argv):
    stop_caller_vm(caller)
    arch = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    extra_cmd = "f2fs.cpunk=1"
    i = 0
    while i < len(argv):
        if argv[i] in ("aarch64", "x86_64"):
            arch = argv[i]
        elif argv[i] == "--cmdline":
            extra_cmd = argv[i + 1]; i += 1
        else:
            sys.exit(__doc__)
        i += 1
    out = os.environ.get("F2FSCM_OUT", f"/tmp/f2fscm-{arch}{('-' + caller) if caller else ''}")
    os.makedirs(out, exist_ok=True)
    img = os.path.join(out, "cm.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    relabel(img)
    pci = "virtio-blk-pci,drive=cmdisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    os.environ["LEANDROS_QEMU_EXTRA"] = (os.environ.get("LEANDROS_QEMU_EXTRA", "") +
                                         f" -drive if=none,id=cmdisk,format=raw,file={img} -device {pci}").strip()
    src = os.path.join(REPO, f"leandros-limine-{arch}.img")
    boot_copy = os.path.join(out, f"boot-{arch}.img")
    fails = []

    def boot(cmdline):
        if os.path.exists(boot_copy):
            os.unlink(boot_copy)
        if cmdline:
            subprocess.run(["cp", "-c", src, boot_copy], check=False, stderr=subprocess.DEVNULL)
            if not os.path.exists(boot_copy):
                import shutil
                shutil.copyfile(src, boot_copy)
            subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), boot_copy, cmdline],
                           check=True, stdout=subprocess.DEVNULL)
            os.environ["LEANDROS_DISK_IMAGE"] = boot_copy
        else:
            os.environ.pop("LEANDROS_DISK_IMAGE", None)
        driver.cmd_start(arch)
        driver.cmd_login("root", "root", timeout=int(90 * driver.wait_scale(arch)))

    def run(c, t=120):
        return driver.serial_run(c, timeout=t, arch=arch)["output"]

    def check(name, cond, detail=""):
        print(f"{name}: {'PASS' if cond else 'FAIL'} {detail}", flush=True)
        if not cond:
            fails.append(name)

    # ---- boot 1: the unknown-outcome commit, then the acknowledged fsync ----
    boot(extra_cmd)
    r = run("mkdir -p /mnt/cm && mount -t f2fs LABEL=f2fscm-test /mnt/cm && echo MOUNT_OK", 120)
    if "MOUNT_OK" not in r:
        print("f2fscommittest: FAIL mount " + r[-200:])
        return 1
    ser = driver._Serial()
    try:
        ser.pump(0.2)
        start = len(ser.buf)
        driver._type_line(ser, "/bin/f2fstest commitunk /mnt/cm", 10.0 * driver.wait_scale(arch))
        ser.pump(120, until=lambda b: b"CLAIM b" in b[start:] or b"FAIL" in b[start:] or b"FSYNC b err" in b[start:])
        text = driver._lrc_clean(ser.buf[start:], True)
    finally:
        ser.close()
    pid = driver._qemu_pid()
    if pid:
        os.kill(pid, signal.SIGKILL)
        time.sleep(1)
    driver.cmd_stop()
    claimed = "CLAIM b" in text
    print("  workload: " + " | ".join(l.strip() for l in text.splitlines() if l.strip())[-200:], flush=True)
    check("boot1: the fsync of b was acknowledged", claimed)

    # ---- boot 2: the claim must have survived ----
    boot(None)
    r = run("mkdir -p /mnt/cm && mount -t f2fs LABEL=f2fscm-test /mnt/cm && echo MOUNT_OK", 120)
    check("boot2: mount", "MOUNT_OK" in r, r[-100:])
    s = run("/bin/f2fstest commitscan /mnt/cm", 60)
    check("boot2: acknowledged fsync survived the kill", "SCAN b ok" in s, s.strip()[-80:])
    run("umount /mnt/cm", 60)
    driver.cmd_stop()
    print("f2fscommittest: " + ("PASS" if not fails else f"FAIL {fails}"))
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
