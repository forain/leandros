#!/usr/bin/env python3
"""Build a Raspberry Pi 5 LeandrOS USB-root stick: partitions only, no ESP.

The Pi boots from Batocera's (or the card's) firmware files and kernel; this
stick only carries the root filesystem the kernel finds by PARTUUID.

  p1  root  type 0x83 / Linux data, f2fs from the rpi5 root image
            (built for LEANDROS_BOARD=rpi5, label leandros-root)
  p2  home  type 0x83 / Linux home, f2fs "leandros-home" (fresh, seeded)
  p3  log   type 0xDA / GUID 042B7470-..., 64 MiB raw kernel log

No Limine ESP, no Wi-Fi credentials, nothing from the host user is copied.
Prints `PARTUUID=<root partuuid>` on the last stdout line.

Usage:
  write-usb-rpi5.py --image OUT.img [--root-image F] [--gpt] [--force]
  write-usb-rpi5.py --device /dev/sdX --confirm-size BYTES [--root-image F] [--gpt]

Default target is an image FILE (refuses to overwrite one without --force).
A block device is refused unless --device is given AND it is a whole USB disk,
not mounted (nor any partition), big enough, and its size in bytes equals
--confirm-size. Environment: LEANDROS_USB_HOME_MIB (default 8192),
LEANDROS_USB_ROOT_MIB (minimum root partition, default 16384),
LEANDROS_RPI5_ROOT_IMG (default f2fs-data0-aarch64.img).
"""
import argparse, os, stat, struct, subprocess, sys, tempfile, uuid, zlib

SECTOR = 512
MIB = 1 << 20
LOG_MIB = 64
F2FS_MAGIC = 0xF2F52010
HERE = os.path.dirname(os.path.abspath(__file__))


def die(msg):
    sys.exit("write-usb-rpi5: " + msg)


def align_up(n, a=MIB):
    return (n + a - 1) // a * a


def f2fs_ok(path):
    with open(path, "rb") as f:
        f.seek(1024)
        b = f.read(4)
    return len(b) == 4 and struct.unpack("<I", b)[0] == F2FS_MAGIC


def layout(root_bytes, home_bytes):
    root_start = MIB // SECTOR
    root_len = align_up(root_bytes) // SECTOR
    home_start = root_start + root_len
    home_len = align_up(home_bytes) // SECTOR
    log_start = home_start + home_len
    log_len = LOG_MIB * MIB // SECTOR
    total = log_start + log_len + (MIB // SECTOR)   # 1 MiB tail holds the GPT backup
    return dict(root_start=root_start, root_len=root_len, home_start=home_start,
                home_len=home_len, log_start=log_start, log_len=log_len, total=total)


def mbr_entry(ptype, start, length):
    chs = b"\xfe\xff\xff"
    return struct.pack("<B3sB3sII", 0, chs, ptype, chs, start, length)


def write_table(f, L, gpt):
    """Write the partition table; return the root PARTUUID text."""
    total = L["total"]
    mbr = bytearray(512)
    sig = 0
    while sig == 0:
        sig = struct.unpack("<I", os.urandom(4))[0]
    mbr[440:444] = struct.pack("<I", sig)
    if not gpt:
        mbr[446:462] = mbr_entry(0x83, L["root_start"], L["root_len"])
        mbr[462:478] = mbr_entry(0x83, L["home_start"], L["home_len"])
        mbr[478:494] = mbr_entry(0xDA, L["log_start"], L["log_len"])
        partuuid = "%08x-01" % sig
    else:
        mbr[446:462] = mbr_entry(0xEE, 1, min(total - 1, 0xFFFFFFFF))
    mbr[510:512] = b"\x55\xaa"
    f.seek(0)
    f.write(mbr)
    if not gpt:
        return partuuid

    def part(ptype, first, length, name, guid):
        return struct.pack("<16s16sQQQ72s", ptype.bytes_le, guid.bytes_le, first,
                           first + length - 1, 0, name.encode("utf-16-le"))
    root_guid = uuid.uuid4()
    entries = bytearray(128 * 128)
    entries[0:128] = part(uuid.UUID("0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
                          L["root_start"], L["root_len"], "LEANDROS root", root_guid)
    entries[128:256] = part(uuid.UUID("933AC7E1-2EB4-4F13-B844-0E14E2AEF915"),
                            L["home_start"], L["home_len"], "LEANDROS home", uuid.uuid4())
    entries[256:384] = part(uuid.UUID("042B7470-1302-4E61-9347-A0130B276754"),
                            L["log_start"], L["log_len"], "LEANDROS-LOG", uuid.uuid4())
    crc = zlib.crc32(entries)
    disk_guid = uuid.uuid4().bytes_le

    def header(my, other, ent_lba):
        h = bytearray(92)
        h[0:8] = b"EFI PART"
        struct.pack_into("<IIIIQQQQ", h, 8, 0x00010000, 92, 0, 0, my, other, 34, total - 34)
        h[56:72] = disk_guid
        struct.pack_into("<QIII", h, 72, ent_lba, 128, 128, crc)
        struct.pack_into("<I", h, 16, zlib.crc32(h))
        return bytes(h).ljust(512, b"\0")
    f.seek(512); f.write(header(1, total - 1, 2))
    f.seek(1024); f.write(entries)
    f.seek((total - 33) * SECTOR); f.write(entries)
    f.seek((total - 1) * SECTOR); f.write(header(total - 1, 1, total - 33))
    return str(root_guid)


def copy_sparse(src, dst, dst_off, chunk=MIB):
    with open(src, "rb") as s:
        pos = 0
        while True:
            b = s.read(chunk)
            if not b:
                break
            if b.count(0) != len(b):
                dst.seek(dst_off + pos)
                dst.write(b)
            pos += len(b)


def build(out_path, root_img, home_mib, root_min_mib, gpt):
    root_bytes = max(os.path.getsize(root_img), root_min_mib * MIB)
    L = layout(root_bytes, home_mib * MIB)
    with tempfile.TemporaryDirectory() as td:
        head = os.path.join(td, "home-head.img")
        subprocess.run([sys.executable, os.path.join(HERE, "mkfs-f2fs-home.py"), "head", head,
                        "--size", str(L["home_len"] * SECTOR)], check=True,
                       stdout=subprocess.DEVNULL)
        with open(out_path, "wb") as f:
            f.truncate(L["total"] * SECTOR)
            pu = write_table(f, L, gpt)
            copy_sparse(root_img, f, L["root_start"] * SECTOR)
            copy_sparse(head, f, L["home_start"] * SECTOR)
    return pu, L


def sysfs_name(dev):
    return os.path.basename(os.path.realpath(dev))


def check_device(dev, confirm, need_bytes):
    st = os.stat(dev)
    if not stat.S_ISBLK(st.st_mode):
        die("%s is not a block device" % dev)
    name = sysfs_name(dev)
    sysdir = "/sys/block/" + name
    if not os.path.isdir(sysdir):
        die("%s is not a whole disk (no %s); pass the disk, not a partition" % (dev, sysdir))
    if "/usb" not in os.path.realpath(sysdir):
        die("%s is not a USB disk (sysfs path %s)" % (dev, os.path.realpath(sysdir)))
    with open(sysdir + "/size") as f:
        size = int(f.read()) * SECTOR
    if size != confirm:
        die("%s is %d bytes but --confirm-size says %d" % (dev, size, confirm))
    if size < need_bytes:
        die("%s is too small: %d bytes, image needs %d" % (dev, size, need_bytes))
    with open("/proc/mounts") as f:
        for line in f:
            src = line.split()[0]
            if src.startswith("/dev/") and sysfs_name(src).startswith(name):
                die("%s is mounted (%s)" % (src, line.split()[1]))
    return size


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--image")
    ap.add_argument("--device")
    ap.add_argument("--confirm-size", type=int)
    ap.add_argument("--root-image", default=os.environ.get(
        "LEANDROS_RPI5_ROOT_IMG", os.path.join(os.path.dirname(HERE), "f2fs-data0-aarch64.img")))
    ap.add_argument("--gpt", action="store_true")
    ap.add_argument("--force", action="store_true")
    a = ap.parse_args()
    home_mib = int(os.environ.get("LEANDROS_USB_HOME_MIB", "8192"))
    root_min = int(os.environ.get("LEANDROS_USB_ROOT_MIB", "16384"))
    if bool(a.image) == bool(a.device):
        die("give exactly one of --image FILE or --device DEV")
    if not os.path.isfile(a.root_image):
        die("root image %s not found (build with LEANDROS_BOARD=rpi5)" % a.root_image)
    if not f2fs_ok(a.root_image):
        die("%s is not an f2fs image" % a.root_image)
    if a.image:
        if os.path.exists(a.image):
            if stat.S_ISBLK(os.stat(a.image).st_mode):
                die("%s is a block device; use --device with its checks" % a.image)
            if not a.force:
                die("%s exists (use --force to replace)" % a.image)
        pu, _ = build(a.image, a.root_image, home_mib, root_min, a.gpt)
        print("wrote image %s" % a.image)
    else:
        if a.confirm_size is None:
            die("--device needs --confirm-size <exact disk size in bytes>")
        need = layout(max(os.path.getsize(a.root_image), root_min * MIB), home_mib * MIB)["total"] * SECTOR
        check_device(a.device, a.confirm_size, need)
        with tempfile.TemporaryDirectory() as td:
            img = os.path.join(td, "stick.img")
            pu, _ = build(img, a.root_image, home_mib, root_min, a.gpt)
            subprocess.run(["dd", "if=" + img, "of=" + a.device, "bs=4M", "conv=fsync,sparse"], check=True)
        print("wrote device %s" % a.device)
    print("PARTUUID=%s" % pu)


main()
