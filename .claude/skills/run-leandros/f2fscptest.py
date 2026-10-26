#!/usr/bin/env python3
"""f2fscptest.py [aarch64|x86_64]

Checkpoint-pack validation of f2fs (lane f2fsmisc), on a fresh 64 MiB volume
attached as an extra disk (run id `<caller>-f2fscp`). The volume is written
by the kernel (several checkpoints), then edited on the host between boots:

  mask     an unknown bit is set in the newest pack's feature mask (u32 at
           commit-block offset 3360, CRC recomputed): the mount must succeed
           read-only with the "unknown features" log line, a write must fail,
           the old data must read back; fsck-f2fs.py reports the unknown bit.
  onebad   the newest pack's CRC is corrupted: the mount must use the other
           pack (data of the earlier checkpoint present, volume writable).
  bothbad  both packs' CRCs are corrupted: the kernel must not panic; it
           logs "both checkpoint packs fail their checks" and mounts the
           newer (or refuses); the machine stays alive either way.
  legacy   a fresh volume whose superblock `init_version` (both copies) is
           blanked, i.e. a legacy LeandrOS-format volume: the mount must be
           refused with the "unsupported legacy LeandrOS f2fs format" log
           line, no panic, and the image must be byte-for-byte unchanged.
Exit 0 iff every scenario passes.
"""
import hashlib
import importlib.util
import os
import struct
import subprocess
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
LABEL = "f2fscp-test"

caller = os.environ.get("LEANDROS_RUN_ID", "")
os.environ["LEANDROS_RUN_ID"] = f"{caller}-f2fscp" if caller else "f2fscp"
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

INCOMPAT_OFF = 3360


def relabel(img):
    name = LABEL.encode("utf-16-le")
    with open(img, "r+b") as f:
        for base in (0, 4096):
            f.seek(base + 1024 + 124)
            f.write(name + bytes(1024 - len(name)))


def packs(img):
    """(block number of pack 0 / 1 commit block, raw blocks) via the fsck parser."""
    v = fsckmod.Vol(img)
    b0, b1 = v.cp_blk, v.cp_blk + v.pack
    out = []
    for b in (b0, b1):
        out.append((b, bytearray(v.blk(b))))
    return out


def patch(img, blkno, buf):
    with open(img, "r+b") as f:
        f.seek(blkno * 4096)
        f.write(bytes(buf))


def recrc(buf):
    if struct.unpack_from("<I", buf, 164)[0] == 4092:
        struct.pack_into("<I", buf, 4092, fsckmod.linux_crc(buf[:4092]))
    else:
        struct.pack_into("<I", buf, 4092, zlib.crc32(bytes(buf[:4092])) & 0xFFFFFFFF)


def fsck_out(img):
    r = subprocess.run([sys.executable, os.path.join(REPO, "scripts", "fsck-f2fs.py"), img],
                       capture_output=True, text=True, timeout=600)
    return r.returncode, r.stdout


def main(argv):
    stop_caller_vm(caller)
    arch = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    for a in argv:
        if a in ("aarch64", "x86_64"):
            arch = a
        else:
            sys.exit(__doc__)
    out = os.environ.get("F2FSCP_OUT", f"/tmp/f2fscp-{arch}{('-' + caller) if caller else ''}")
    os.makedirs(out, exist_ok=True)
    img = os.path.join(out, "cp.img")
    if os.path.exists(img):
        os.unlink(img)
    def mkfs():
        if os.path.exists(img):
            os.unlink(img)
        subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                        "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
        relabel(img)
    mkfs()
    pci = "virtio-blk-pci,drive=cpdisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    os.environ["LEANDROS_QEMU_EXTRA"] = (os.environ.get("LEANDROS_QEMU_EXTRA", "") +
                                         f" -drive if=none,id=cpdisk,format=raw,file={img} -device {pci}").strip()
    fails = []

    def boot():
        driver.cmd_start(arch)
        driver.cmd_login("root", "root", timeout=int(90 * driver.wait_scale(arch)))

    def run(c, t=120):
        return driver.serial_run(c, timeout=t, arch=arch)["output"]

    def mount(extra=""):
        return run(f"mkdir -p /mnt/cp && mount -t f2fs LABEL={LABEL} /mnt/cp && echo MOUNT_OK || echo MOUNT_FAIL", 120)

    def dmesg():
        # The boot's F2FS lines go to the serial console (the guest has no grep).
        run("echo SYNC", 10)
        lf = f"/tmp/leandros-{os.environ['LEANDROS_RUN_ID']}-serial-full.log"
        o = open(lf, errors="replace").read() if os.path.exists(lf) else ""
        return "\n".join(l for l in o.splitlines() if "F2FS" in l)

    def check(name, cond, detail=""):
        print(f"{name}: {'PASS' if cond else 'FAIL'} {detail}", flush=True)
        if not cond:
            fails.append(name)

    def seed_volume():
        """Boot, write file A, checkpoint, write file B, checkpoint, umount."""
        boot()
        r = mount()
        assert "MOUNT_OK" in r, r[-300:]
        run("echo alpha > /mnt/cp/a.txt && sync && echo beta > /mnt/cp/b.txt && sync && echo gamma > /mnt/cp/c.txt && sync "
            "&& umount /mnt/cp && echo UMOUNT_OK")
        driver.cmd_stop()

    def newest(img):
        (b0, p0), (b1, p1) = packs(img)
        j = [struct.unpack_from("<I", p, fsckmod.CPJ_MAGIC_OFF)[0] == fsckmod.CPJ_MAGIC for p in (p0, p1)]
        v0, v1 = struct.unpack_from("<Q", p0, 0)[0], struct.unpack_from("<Q", p1, 0)[0]
        return (b0, p0, b1, p1, j, v0, v1)

    # ---- mask ----
    seed_volume()
    b0, p0, b1, p1, j, v0, v1 = newest(img)
    check("seed: both packs journaled with valid CRC", all(j) and fsckmod.cp_crc_ok(p0) and fsckmod.cp_crc_ok(p1),
          f"(ver {v0}/{v1})")
    nb, nbuf = (b1, p1) if v1 > v0 else (b0, p0)
    ob, obuf = (b0, p0) if v1 > v0 else (b1, p1)
    struct.pack_into("<I", nbuf, INCOMPAT_OFF, 0x80000001)
    recrc(nbuf)
    patch(img, nb, nbuf)
    rc, o = fsck_out(img)
    check("mask: fsck-f2fs reports the unknown bit", "unknown" in o.lower() and "0x80000000" in o.lower(), o.strip()[-120:])
    boot()
    r = mount()
    lg = dmesg()
    check("mask: mounts", "MOUNT_OK" in r)
    check("mask: unknown-features log line", "unknown features" in lg and "read-only" in lg, lg[-200:])
    w = run("echo x > /mnt/cp/new.txt 2>&1; echo WRC=$?; cat /mnt/cp/c.txt")
    check("mask: write refused", "WRC=0" not in w, w[-120:])
    check("mask: data readable", "gamma" in w)
    driver.cmd_stop()
    # restore for the next scenario
    struct.pack_into("<I", nbuf, INCOMPAT_OFF, 0)
    recrc(nbuf)
    patch(img, nb, nbuf)

    # ---- one bad ----
    seed_volume()
    b0, p0, b1, p1, j, v0, v1 = newest(img)
    nb, nbuf = (b1, p1) if v1 > v0 else (b0, p0)
    nbuf[4092] ^= 0xFF
    patch(img, nb, nbuf)
    boot()
    r = mount()
    check("onebad: mounts from the other pack", "MOUNT_OK" in r, r[-100:])
    w = run("cat /mnt/cp/a.txt; echo y > /mnt/cp/after.txt && sync && echo WROTE")
    check("onebad: earlier data present, volume writable", "alpha" in w and "WROTE" in w, w[-120:])
    run("umount /mnt/cp")
    driver.cmd_stop()

    # ---- both bad ----
    seed_volume()
    b0, p0, b1, p1, j, v0, v1 = newest(img)
    for b, p in ((b0, p0), (b1, p1)):
        p[4092] ^= 0xFF
        patch(img, b, p)
    boot()
    r = mount()
    lg = dmesg()
    alive = "ALIVE" in run("echo ALIVE")
    check("bothbad: no panic, machine alive", alive and "panic" not in lg.lower(), r[-80:])
    check("bothbad: logged", "both checkpoint packs fail their checks" in lg or "MOUNT_FAIL" in r, lg[-160:])
    driver.cmd_stop()

    # ---- legacy LeandrOS format: refused, untouched ----
    mkfs()
    with open(img, "r+b") as f:
        for base in (0, 4096):
            f.seek(base + 1024 + 1924)  # f2fs_super_block.init_version[256]
            f.write(bytes(256))
    h0 = hashlib.sha256(open(img, "rb").read()).hexdigest()
    boot()
    r = mount()
    lg = dmesg()
    alive = "ALIVE" in run("echo ALIVE")
    check("legacy: refused", "MOUNT_FAIL" in r and "MOUNT_OK" not in r, r[-80:])
    check("legacy: logged", "unsupported legacy LeandrOS f2fs format" in lg, lg[-160:])
    check("legacy: no panic, machine alive", alive and "panic" not in lg.lower())
    driver.cmd_stop()
    check("legacy: image untouched", hashlib.sha256(open(img, "rb").read()).hexdigest() == h0)

    print("f2fscptest: " + ("PASS" if not fails else f"FAIL {fails}"))
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
