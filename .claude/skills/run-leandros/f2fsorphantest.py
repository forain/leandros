#!/usr/bin/env python3
"""f2fsorphantest.py [aarch64|x86_64]

Persisted orphan list of f2fs (lane f2fsorphan), on a fresh 64 MiB volume
attached as an extra disk (run id `<caller>-f2fsorph`):

  1. boot, mount; `f2fstest holdorphan` writes a 2 MiB file, unlinks it
     while open, syncs (a checkpoint is cut while the file is unlinked but
     open) and keeps it open; then the machine is powered off (QEMU quit);
  2. fsck of the crashed image passes, and with --strict-leaks too (the
     listed orphan owns its blocks), and it reports the orphan list;
  3. boot, mount: the log shows "orphan inodes reaped at mount: 1"; umount;
  4. fsck --strict-leaks passes: the 2 MiB came back. Before the fix the
     inode survived with no name and its blocks leaked forever.
  5. (lane f2fsorph3) hold a second orphan and power off again; boot, mount
     -o ro, umount: the on-disk list is still "orphan list: 1"; mount -o ro,
     `mount -o remount,rw`, write + sync, umount: the log shows "orphan
     inodes reaped at remount: 1" and fsck --strict-leaks passes. Before the
     fix the first read-write checkpoint dropped the list and the inode leaked.
Exit 0 iff every step passes.
"""
import importlib.util
import os
import hostclean
import struct
import subprocess
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
LABEL = "f2fsorph-test"

caller = os.environ.get("LEANDROS_RUN_ID", "")
os.environ["LEANDROS_RUN_ID"] = f"{caller}-f2fsorph" if caller else "f2fsorph"
os.environ.pop("LEANDROS_USB_STICK", None)
sys.path.insert(0, HERE)
import driver  # noqa: E402


def stop_caller_vm(caller):
    """Under `runtests host:` the caller's VM still holds the write lock on the
    root image, so this test's own QEMU would exit at once ("Failed to get
    write lock", seen as "shell prompt not seen"). Stop it first."""
    if caller:
        subprocess.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "driver.py"), "stop"],
                       env=dict(os.environ, LEANDROS_RUN_ID=caller), capture_output=True, timeout=120)

spec = importlib.util.spec_from_file_location("fsckf2fs", os.path.join(REPO, "scripts", "fsck-f2fs.py"))
fsckmod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fsckmod)



def relabel(img, label=LABEL):
    name = label.encode("utf-16-le")
    with open(img, "r+b") as f:
        for base in (0, 4096):
            f.seek(base + 1024 + 124)
            f.write(name + bytes(1024 - len(name)))



def fsck_out(img, strict=False):
    r = subprocess.run([sys.executable, os.path.join(REPO, "scripts", "fsck-f2fs.py"), img]
                       + (["--strict-leaks"] if strict else []), capture_output=True, text=True, timeout=600)
    return r.returncode, r.stdout


def main(argv):
    stop_caller_vm(caller)
    arch = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    for a in argv:
        if a in ("aarch64", "x86_64"):
            arch = a
        else:
            sys.exit(__doc__)
    out = os.environ.get("F2FSORPH_OUT", f"/tmp/f2fsorph-{arch}{('-' + caller) if caller else ''}")
    os.makedirs(out, exist_ok=True)
    hostclean.track(out, glob="/tmp/f2fsorph-*")
    img = os.path.join(out, "orph.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    relabel(img)
    extra0 = os.environ.get("LEANDROS_QEMU_EXTRA", "")
    pci = "virtio-blk-pci,drive=orphdisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    os.environ["LEANDROS_QEMU_EXTRA"] = (os.environ.get("LEANDROS_QEMU_EXTRA", "") +
                                         f" -drive if=none,id=orphdisk,format=raw,file={img} -device {pci}").strip()
    fails = []

    def boot():
        driver.cmd_start(arch)
        driver.cmd_login("root", "root", timeout=int(90 * driver.wait_scale(arch)))

    def run(c, t=120):
        return driver.serial_run(c, timeout=t, arch=arch)["output"]

    def mount(opt=""):
        return run(f"mkdir -p /mnt/op && mount -t f2fs {opt}LABEL={LABEL} /mnt/op && echo MOUNT_OK || echo MOUNT_FAIL", 120)

    def dmesg():
        run("echo SYNC", 10)
        lf = f"/tmp/leandros-{os.environ['LEANDROS_RUN_ID']}-serial-full.log"
        o = open(lf, errors="replace").read() if os.path.exists(lf) else ""
        return "\n".join(l for l in o.splitlines() if "F2FS" in l)

    def check(name, cond, detail=""):
        print(f"{name}: {'PASS' if cond else 'FAIL'} {detail}", flush=True)
        if not cond:
            fails.append(name)

    boot()
    check("mount", "MOUNT_OK" in mount())
    run("( /bin/f2fstest holdorphan /mnt/op > /tmp/ho.log 2>&1 & )", 20)  # the wrapper appends "; ..."
    r = ""
    for _ in range(12):
        r = run("sleep 5; cat /tmp/ho.log", 30)
        if "HELD" in r or "FAIL" in r:
            break
    check("2 MiB file unlinked while open + sync", "HELD" in r, r.strip()[-160:])
    driver.cmd_stop()  # QEMU quit: a power cut, no umount

    rc, o = fsck_out(img)
    check("crashed image: fsck", rc == 0, o.strip()[-200:])
    check("crashed image: orphan list reported", "orphan list: 1" in o, "")
    rc, o = fsck_out(img, strict=True)
    check("crashed image: fsck --strict-leaks", rc == 0, o.strip()[-200:])

    boot()
    check("remount", "MOUNT_OK" in mount())
    log = dmesg()
    check("orphan reaped at mount", "orphan inodes reaped at mount: 1" in log,
          "\n".join(l for l in log.splitlines() if "orphan" in l)[-200:])
    r = run("umount /mnt/op && echo UMOUNT_OK", 120)
    check("umount", "UMOUNT_OK" in r)
    driver.cmd_stop()
    rc, o = fsck_out(img, strict=True)
    check("after remount: fsck --strict-leaks", rc == 0, o.strip()[-200:])

    # Phase 2: the list must survive a read-only mount and be reaped at the
    # read-write remount.
    boot()
    check("ro phase: mount", "MOUNT_OK" in mount())
    run("( /bin/f2fstest holdorphan /mnt/op > /tmp/ho.log 2>&1 & )", 20)
    r = ""
    for _ in range(12):
        r = run("sleep 5; cat /tmp/ho.log", 30)
        if "HELD" in r or "FAIL" in r:
            break
    check("ro phase: orphan held", "HELD" in r, r.strip()[-160:])
    driver.cmd_stop()
    rc, o = fsck_out(img)
    check("ro phase: crashed image lists the orphan", rc == 0 and "orphan list: 1" in o, o.strip()[-200:])

    boot()
    check("ro phase: mount -o ro", "MOUNT_OK" in mount("-o ro "))
    r = run("umount /mnt/op && echo UMOUNT_OK", 120)
    check("ro phase: umount ro", "UMOUNT_OK" in r)
    check("ro phase: mount -o ro again", "MOUNT_OK" in mount("-o ro "))
    r = run("mount -o remount,rw /mnt/op && echo RW_OK; dd if=/dev/zero of=/mnt/op/after bs=4096 count=8 && sync && echo W_OK", 120)
    check("ro phase: remount rw + write", "RW_OK" in r and "W_OK" in r, r.strip()[-160:])
    log = dmesg()
    check("ro phase: orphan reaped at remount", "orphan inodes reaped at remount: 1" in log,
          "\n".join(l for l in log.splitlines() if "orphan" in l)[-200:])
    r = run("umount /mnt/op && echo UMOUNT_OK", 120)
    check("ro phase: umount", "UMOUNT_OK" in r)
    driver.cmd_stop()
    rc, o = fsck_out(img, strict=True)
    check("ro phase: fsck --strict-leaks", rc == 0, o.strip()[-200:])

    # Phase 3 (lane homenotdir): stale orphan ids must not reclaim reused nids.
    # Needs more room than the 64 MiB image: its own 512 MiB disk and label.
    label2 = "f2fsreuse-test"
    img2 = os.path.join(out, "reuse.img")
    if os.path.exists(img2):
        os.unlink(img2)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img2,
                    "--size", "512M"], check=True, stdout=subprocess.DEVNULL)
    relabel(img2, label2)
    pci2 = "virtio-blk-pci,drive=reusedisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    os.environ["LEANDROS_QEMU_EXTRA"] = (extra0 + f" -drive if=none,id=reusedisk,format=raw,file={img2} -device {pci2}").strip()
    boot()
    r = run(f"mkdir -p /mnt/or && mount -t f2fs LABEL={label2} /mnt/or && echo MOUNT_OK || echo MOUNT_FAIL", 120)
    check("orphanreuse: mount", "MOUNT_OK" in r)
    r = run("/bin/f2fstest orphanreuse /mnt/or/orphanreuse; echo RC=$?", 900)
    check("orphanreuse: guest", "orphanreuse: PASS" in r and "FAIL" not in r, r.strip()[-200:])
    r = run("umount /mnt/or && echo UMOUNT_OK", 120)
    check("orphanreuse: umount", "UMOUNT_OK" in r)
    driver.cmd_stop()
    rc, o = fsck_out(img2, strict=True)
    rc2, o2 = subprocess.run([sys.executable, os.path.join(REPO, "scripts", "fsck-f2fs.py"), img2, "--strict-links"],
                             capture_output=True, text=True, timeout=600).returncode, ""
    check("orphanreuse: fsck --strict-leaks", rc == 0, o.strip()[-300:])
    check("orphanreuse: fsck --strict-links", rc2 == 0, "")

    print("f2fsorphantest:", "PASS" if not fails else f"FAIL ({', '.join(fails)})", flush=True)
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(hostclean.run(main, sys.argv[1:]))
