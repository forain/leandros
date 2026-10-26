#!/usr/bin/env python3
"""The persistent /home volume: create it, keep it, find it.

/home lives on its own f2fs volume, labelled `leandros-home`, so that rebuilding
the root image (scripts/build-all.sh rewrites f2fs-data0-<arch>.img every time)
never touches user data: Claude Code credentials, the Firefox profile, COSMIC
settings. /etc/fstab mounts it by label (`LABEL=leandros-home /home ...`), and
the kernel resolves the label on whichever disk carries it: QEMU's third
virtio disk (vdc, f2fs-data1-<arch>.img) or the home partition that
scripts/write-usb.sh puts on a USB stick.

Commands:
  create <img> [--size SIZE]        write a fresh, seeded home volume (sparse)
  ensure <img> [--size SIZE] [--reset]
                                    create <img> when it is missing or is not a
                                    home volume (an old root clone); keep it
                                    otherwise. --reset recreates it regardless.
  head <out> --size SIZE            write only the volume's head (metadata and
                                    seeded blocks; the rest of a fresh volume is
                                    zeros) for embedding in a disk image
  is-home <img|dev> [--offset BYTES]
                                    exit 0 when an f2fs home volume starts there
  find-part <img|dev>               print `<start sector> <sectors>` of the
                                    partition holding the home volume (MBR or
                                    GPT, 512-byte sectors); exit 1 if none
  clone <src> <dst>                 copy-on-write copy (APFS clone / reflink,
                                    else a sparse copy) for a throwaway run

SIZE accepts K/M/G/T suffixes (powers of 1024). The default is 8G: a sparse
file, so it costs only what the guest writes.

The layout is the one scripts/mkfs-f2fs-populated.py writes and
servers/f2fs/src/lib.rs reads (4 KiB blocks, 2 MiB segments, one checkpoint
pack per segment, NAT 2 segments), with two differences: the SIT grows with
the volume (one SIT block covers 55 segments, i.e. 110 MiB), and SIT entries
are indexed by main-area segment number, which is how the server reads them.

Inodes, nodes and directories use Linux's on-disk layout ("lx", the
`LeandrOS lxnode2` marker in the superblock's version and init_version;
scripts/f2fslx.py), the only format the kernel mounts. `ensure` never
rewrites an existing lx volume; a home volume in the old LeandrOS-private
layout (made before lane lxdefault) is moved aside to `<img>.legacy` and
replaced by a fresh one.

The seed is the home skeleton the root image also carries (the fallback when
no home volume is attached): /home/leandro 0700 1000:1000 and
/home/cosmic-greeter 0700 990:990 with .config/.cache/.local. Accounts added
later get their home from init, which creates any missing /home/<user> listed
in /etc/passwd at boot (userland/init: seed_home_dirs).
"""
import os
import shutil
import struct
import subprocess
import sys
import uuid

import f2fslx

LABEL = "leandros-home"
DEFAULT_SIZE = 8 << 30

BLOCK = 4096
BPS = 512                      # blocks per segment
F2FS_MAGIC = 0xF2F52010
CP_UMOUNT_FLAG = 0x1
NAT_ENTRY_SIZE = 9
SIT_ENTRY_SIZE = 74
SIT_PER_BLK = BLOCK // SIT_ENTRY_SIZE      # 55
MAX_ACTIVE_LOGS = 8
DT_DIR = 4
SB_LABEL_OFF = 124                         # __le16 volume_name[512]

# (path relative to the volume root, mode, uid, gid). Parents first.
SKELETON = [
    ("leandro",                0o040700, 1000, 1000),
    ("cosmic-greeter",         0o040700, 990, 990),
    ("cosmic-greeter/.config", 0o040700, 990, 990),
    ("cosmic-greeter/.cache",  0o040700, 990, 990),
    ("cosmic-greeter/.local",  0o040700, 990, 990),
]


def die(msg):
    sys.exit(f"mkfs-f2fs-home: {msg}")


def parse_size(s):
    mult = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30, "T": 1 << 40}
    s = s.strip().upper().rstrip("B").rstrip("I")
    if s and s[-1] in mult:
        return int(float(s[:-1]) * mult[s[-1]])
    return int(s)


def w16(b, o, v): struct.pack_into("<H", b, o, v & 0xFFFF)
def w32(b, o, v): struct.pack_into("<I", b, o, v & 0xFFFFFFFF)
def w64(b, o, v): struct.pack_into("<Q", b, o, v)


def build(size):
    """{block number: 4 KiB bytes} of every non-zero block of a fresh volume,
    and the volume's length in blocks."""
    total_segs = size // (BLOCK * BPS)
    if total_segs < 32:
        die(f"{size} bytes is too small for a home volume (64 MiB minimum)")
    sit_blocks = -(-total_segs // SIT_PER_BLK)
    # Two SIT copies (Linux mirrors SIT and NAT; lane f2fslayout).
    sit_segs = 2 * -(-sit_blocks // BPS)
    ckpt_segs, nat_segs = 2, 2
    ssa_segs = f2fslx.ssa_segs_for(total_segs, 1 + ckpt_segs + sit_segs + nat_segs)
    seg0 = BPS
    cp_blk = seg0
    sit_blk = cp_blk + ckpt_segs * BPS
    nat_blk = sit_blk + sit_segs * BPS
    ssa_blk = nat_blk + nat_segs * BPS
    main_blk = ssa_blk + ssa_segs * BPS
    meta_segs = 1 + ckpt_segs + sit_segs + nat_segs + ssa_segs
    main_segs = total_segs - meta_segs
    total_blocks = total_segs * BPS

    blocks = {}

    def blk(n):
        if n not in blocks:
            blocks[n] = bytearray(BLOCK)
        return blocks[n]

    # Superblock, both copies (block 0 and block 1, at byte 1024 of each).
    vol_uuid = uuid.uuid4().bytes
    for base in (0, 1):
        b = blk(base)
        s = 1024
        w32(b, s + 0, F2FS_MAGIC)
        w16(b, s + 4, 1)
        w16(b, s + 6, 12)
        w32(b, s + 8, 9)
        w32(b, s + 12, 3)
        w32(b, s + 16, 12)
        w32(b, s + 20, 9)
        w32(b, s + 24, 1)
        w32(b, s + 28, 1)
        w64(b, s + 36, total_blocks)
        w32(b, s + 44, main_segs)
        w32(b, s + 48, total_segs - 1)     # from segment0_blkaddr on
        w32(b, s + 52, ckpt_segs)
        w32(b, s + 56, sit_segs)
        w32(b, s + 60, nat_segs)
        w32(b, s + 64, ssa_segs)
        w32(b, s + 68, main_segs)
        w32(b, s + 72, seg0)
        w32(b, s + 76, cp_blk)
        w32(b, s + 80, sit_blk)
        w32(b, s + 84, nat_blk)
        w32(b, s + 88, ssa_blk)
        w32(b, s + 92, main_blk)
        w32(b, s + 96, 3)        # root ino
        w32(b, s + 100, 1)       # node ino
        w32(b, s + 104, 2)       # meta ino
        b[s + 108:s + 124] = vol_uuid
        name = LABEL.encode("utf-16-le")
        b[s + SB_LABEL_OFF:s + SB_LABEL_OFF + len(name)] = name
        f2fslx.mark_superblock(b, s)

    # Directories: ino 3 is the root, the skeleton follows from ino 4.
    dirs = [("", 0o040755, 0, 0)] + SKELETON
    ino_of = {p: 3 + i for i, (p, _m, _u, _g) in enumerate(dirs)}
    children = {p: [] for p, *_ in dirs}
    for p, *_ in dirs[1:]:
        children[os.path.dirname(p)].append(p)
    n = len(dirs)
    inode_at = {p: main_blk + i for i, (p, *_r) in enumerate(dirs)}
    # Node blocks in main segment 0 (HOT_NODE), dentry blocks in segment 1
    # (HOT_DATA): fsck.f2fs wants a segment to hold one class only.
    data_at = {p: main_blk + BPS + i for i, (p, *_r) in enumerate(dirs)}

    for p, mode, uid, gid in dirs:
        ino = ino_of[p]
        parent = ino_of[os.path.dirname(p)] if p else ino
        ents = [(b".", ino), (b"..", parent)]
        ents += [(os.path.basename(c).encode(), ino_of[c]) for c in children[p]]
        # Linux hash-bucket placement; the skeleton fits bucket 0 of level 0.
        dblocks, depth = f2fslx.place_dentries([(nm, c, DT_DIR) for nm, c in ents])
        assert list(dblocks) == [0] and depth == 1
        blocks[data_at[p]] = dblocks[0]

        node = blk(inode_at[p])
        w16(node, 0, mode)
        w32(node, 4, uid)
        w32(node, 8, gid)
        w32(node, 12, 2 + len(children[p]))      # links: self, parent, each child's ..
        w64(node, 16, BLOCK)                     # size: one dentry block
        w64(node, f2fslx.I_BLOCKS, 2)            # inode + dentry block
        w32(node, f2fslx.I_CURRENT_DEPTH, 1)
        w32(node, f2fslx.I_PINO, parent)
        nm = os.path.basename(p).encode()
        w32(node, f2fslx.I_NAMELEN, len(nm))
        node[f2fslx.I_NAME:f2fslx.I_NAME + len(nm)] = nm
        w32(node, f2fslx.I_ADDR, data_at[p])
        # atime/mtime/ctime = now, not the epoch.
        now = int(os.environ.get("SOURCE_DATE_EPOCH") or __import__("time").time())
        for off in (f2fslx.I_ATIME, f2fslx.I_CTIME, f2fslx.I_MTIME):
            w64(node, off, now)
        f2fslx.write_footer(node, ino, ino, 0, cp_ver=1)

        # NAT entry.
        per = BLOCK // NAT_ENTRY_SIZE
        nb = blk(nat_blk + ino // per)
        o = (ino % per) * NAT_ENTRY_SIZE
        nb[o] = 1
        w32(nb, o + 1, ino)
        w32(nb, o + 5, inode_at[p])

    # NAT entries for the node and meta inodes (Linux keeps them at blkaddr 1).
    per = BLOCK // NAT_ENTRY_SIZE
    for dino in (1, 2):
        nb = blk(nat_blk + dino // per)
        o = (dino % per) * NAT_ENTRY_SIZE
        nb[o] = 0
        w32(nb, o + 1, dino)
        w32(nb, o + 5, 1)

    # Six typed current segments (fsck.f2fs wants one per log): HOT_NODE on
    # segment 0 and HOT_DATA on segment 1 at offset n, the others empty on
    # segments 2..5 (WARM_DATA, COLD_DATA, WARM_NODE, COLD_NODE).
    log_seg = {0: 1, 1: 2, 2: 3, 3: 0, 4: 4, 5: 5}      # log type -> segment
    log_off = {0: n, 3: n}
    seg_type = {seg: t for t, seg in log_seg.items()}
    used = 2 * n
    sb_ = blk(sit_blk)
    for seg, t in seg_type.items():
        used_here = n if seg in (0, 1) else 0
        sb_[seg * SIT_ENTRY_SIZE:(seg + 1) * SIT_ENTRY_SIZE] = \
            f2fslx.sit_entry(used_here, t, range(used_here))

    # SSA: owner of every seeded block, footer = segment class.
    owners = [(ino_of[p], 1, 0) for p, *_r in dirs]
    for seg, t in seg_type.items():
        blocks[ssa_blk + seg] = f2fslx.summary_block(owners if seg in (0, 1) else [], t >= 3)

    # Checkpoint packs: Linux layout, whole pack (512 blocks), six normal
    # summaries at pack-7..pack-2 and the commit block's copy as the last block.
    rsvd, ovp = f2fslx.checkpoint_reserve(ckpt_segs + sit_segs + nat_segs + ssa_segs)
    pack = (ckpt_segs // 2) * BPS
    free_segs = main_segs - 6

    def checkpoint(at, ver):
        c = blk(at)
        w64(c, 0, ver)
        w64(c, 8, (main_segs - ovp) * BPS)     # user_block_count
        w64(c, 16, used)
        w32(c, 24, rsvd)
        w32(c, 28, ovp)
        w32(c, 32, free_segs)
        for t in range(6):
            seg = log_seg[t]
            if t < 3:
                w32(c, 84 + t * 4, seg); w16(c, 116 + t * 2, log_off.get(t, 0))
            else:
                w32(c, 36 + (t - 3) * 4, seg); w16(c, 68 + (t - 3) * 2, log_off.get(t, 0))
        for i in range(6, MAX_ACTIVE_LOGS):
            w32(c, 36 + i * 4, 0xFFFFFFFF)
            w32(c, 84 + i * 4, 0xFFFFFFFF)
        w32(c, 132, CP_UMOUNT_FLAG)
        w32(c, 136, pack)
        w32(c, 140, pack - 7)              # cp_pack_start_sum
        w32(c, 144, n)                     # valid_node_count
        w32(c, 148, n)                     # valid_inode_count
        w32(c, 152, 3 + n)                 # next_free_nid
        w32(c, 156, (sit_segs // 2) * BPS // 8)
        w32(c, 160, (nat_segs // 2) * BPS // 8)
        for t in range(16):
            c[176 + t] = 1                 # alloc_type: SSR, as the kernel writes
        w64(c, 2048, ver)                  # servers/f2fs CP_OWN_VER
        w32(c, 3348, log_seg[0])           # CP_OWN_HEADS: data seg, node seg,
        w32(c, 3352, log_seg[3])           # data blkoff, node blkoff
        w16(c, 3356, n)
        w16(c, 3358, n)
        f2fslx.seal_cp(c)
        for t in range(6):
            ents = owners if t in (0, 3) else []
            blocks[at + pack - 7 + t] = f2fslx.summary_block(ents, t >= 3)
        blocks[at + pack - 1] = bytearray(c)
    checkpoint(cp_blk, 1)
    checkpoint(cp_blk + pack, 0)
    return blocks, total_blocks


def write_volume(path, size, head_only=False):
    blocks, total_blocks = build(size)
    tmp = path + ".tmp"
    with open(tmp, "wb") as f:
        for n in sorted(blocks):
            f.seek(n * BLOCK)
            f.write(blocks[n])
        f.truncate((max(blocks) + 1) * BLOCK if head_only else total_blocks * BLOCK)
    os.replace(tmp, path)
    return total_blocks * BLOCK


def is_lx_volume(path, offset=0):
    """The superblock at `offset` carries the lx marker (or was made by Linux)."""
    with open(path, "rb") as f:
        f.seek(offset + 1024)
        sb = f.read(4096 - 1024)
    return f2fslx.is_lx_superblock(sb) or sb[f2fslx.SB_INIT_VERSION_OFF:].startswith(b"Linux version")


def label_at(path, offset=0):
    """The f2fs volume label at byte `offset` of `path`, or None when there is
    no f2fs superblock there. Reads one sector-aligned 4 KiB block, so it works
    on raw devices too."""
    try:
        with open(path, "rb", buffering=0) as f:
            f.seek(offset)
            b = f.read(BLOCK)
    except OSError:
        return None
    if len(b) < 2048 or struct.unpack_from("<I", b, 1024)[0] != F2FS_MAGIC:
        return None
    raw = b[1024 + SB_LABEL_OFF:1024 + SB_LABEL_OFF + 1024]
    name = raw.decode("utf-16-le", "replace")
    return name.split("\0", 1)[0]


def find_part(path):
    """(start, sectors) of the home partition on a disk image or device."""
    with open(path, "rb", buffering=0) as f:   # raw devices: sector-sized reads only
        mbr = f.read(512)
        if len(mbr) < 512 or mbr[510:512] != b"\x55\xaa":
            return None
        parts = []
        types = [mbr[446 + 16 * i + 4] for i in range(4)]
        if 0xEE in types:
            f.seek(512)
            hdr = f.read(512)
            if hdr[0:8] != b"EFI PART":
                return None
            ent_lba, count, esize = struct.unpack_from("<QII", hdr, 72)
            f.seek(ent_lba * 512)
            raw = f.read(-(-count * esize // 512) * 512)
            for i in range(count):
                e = raw[i * esize:(i + 1) * esize]
                if len(e) < 48 or e[0:16] == bytes(16):
                    continue
                first, last = struct.unpack_from("<QQ", e, 32)
                parts.append((first, last - first + 1))
        else:
            for i in range(4):
                e = mbr[446 + 16 * i:446 + 16 * (i + 1)]
                start, length = struct.unpack_from("<II", e, 8)
                if e[4] and length:
                    parts.append((start, length))
    for start, length in parts:
        if start % 8 == 0 and label_at(path, start * 512) == LABEL:
            return start, length
    return None


def clone(src, dst):
    if os.path.exists(dst):
        os.remove(dst)
    for cmd in (["cp", "-c", src, dst], ["cp", "--reflink=always", src, dst]):
        if subprocess.run(cmd, stderr=subprocess.DEVNULL).returncode == 0:
            return
    if subprocess.run(["cp", "--sparse=always", src, dst],
                      stderr=subprocess.DEVNULL).returncode == 0:
        return
    shutil.copyfile(src, dst)


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    cmd, rest = argv[0], argv[1:]
    size, reset, offset, pos = DEFAULT_SIZE, False, 0, []
    i = 0
    while i < len(rest):
        a = rest[i]
        if a == "--size":
            size = parse_size(rest[i + 1]); i += 2
        elif a == "--reset":
            reset = True; i += 1
        elif a == "--offset":
            offset = int(rest[i + 1]); i += 2
        else:
            pos.append(a); i += 1

    if cmd == "create" and len(pos) == 1:
        n = write_volume(pos[0], size)
        print(f"🏠 Created home volume {pos[0]} ({n >> 20} MiB sparse, label {LABEL})")
    elif cmd == "head" and len(pos) == 1:
        write_volume(pos[0], size, head_only=True)
    elif cmd == "ensure" and len(pos) == 1:
        img = pos[0]
        label = label_at(img) if os.path.exists(img) else None
        if os.path.exists(img) and label == LABEL and not reset:
            if is_lx_volume(img):
                print(f"🏠 Keeping home volume {img} (user data; --reset-home recreates it)")
                return 0
            # The legacy LeandrOS-private layout is no longer mountable (the
            # kernel refuses it). Keep the old file aside, make a fresh one.
            os.replace(img, img + ".legacy")
            print(f"🏠 {img} uses the legacy LeandrOS f2fs format, which is no longer supported;")
            print(f"   moved it to {img}.legacy (delete it when not needed) and creating a fresh volume.")
        if reset and os.path.exists(img):
            print(f"🏠 Resetting home volume {img}: its contents are discarded")
        elif os.path.exists(img):
            what = "an f2fs volume without the home label" if label is not None else "not an f2fs volume"
            print(f"🏠 {img} is {what} (an old /data root clone); replacing it with a")
            print("   home volume, once. Nothing in it was mounted at /home.")
        n = write_volume(img, size)
        print(f"🏠 Created home volume {img} ({n >> 20} MiB sparse, label {LABEL})")
    elif cmd == "is-home" and len(pos) == 1:
        return 0 if label_at(pos[0], offset) == LABEL else 1
    elif cmd == "find-part" and len(pos) == 1:
        r = find_part(pos[0])
        if r is None:
            return 1
        print(f"{r[0]} {r[1]}")
    elif cmd == "clone" and len(pos) == 2:
        clone(pos[0], pos[1])
    else:
        die(f"bad usage: {' '.join(argv)} (see --help)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
