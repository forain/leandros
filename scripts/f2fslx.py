"""The "lx" f2fs node format: Linux's exact on-disk inode, node and dentry
layout (include/linux/f2fs_fs.h), used by volumes whose superblock carries the
`LeandrOS lxnode1` marker. Shared by scripts/mkfs-f2fs-home.py,
scripts/mkfs-f2fs-populated.py and scripts/fsck-f2fs.py; servers/f2fs reads
the same layout when it finds the marker.

Volumes without the marker use the legacy LeandrOS layout (footer at 4076,
i_addr at 364, 1019 slots per node, dentry hash 0, linear directories) and
keep it forever.
"""
import struct

MARKER = b"LeandrOS lxnode1"
SB_VERSION_OFF = 1668          # struct f2fs_super_block: version[256]
SB_INIT_VERSION_OFF = 1924     # init_version[256]; the format flag

# Inode block (struct f2fs_inode).
I_MODE, I_ADVISE, I_INLINE = 0, 2, 3
I_UID, I_GID, I_LINKS, I_SIZE = 4, 8, 12, 16
I_BLOCKS = 24                  # u64, 4 KiB units: inode + node + data blocks
I_ATIME, I_CTIME, I_MTIME = 32, 40, 48
I_ATIME_NS, I_CTIME_NS, I_MTIME_NS = 56, 60, 64
I_GENERATION, I_CURRENT_DEPTH, I_XATTR_NID = 68, 72, 76
I_FLAGS, I_PINO, I_NAMELEN, I_NAME = 80, 84, 88, 92
I_DIR_LEVEL, I_EXT = 347, 348
I_ADDR = 360                   # DEF_ADDRS_PER_INODE (923) slots
ADDRS_PER_INODE = 923
I_NID = 4052                   # i_nid[5]
NAME_LEN = 255

# Inline flags (i_inline). lx volumes we write never set any of them.
F2FS_INLINE_XATTR = 0x01
F2FS_INLINE_DATA = 0x02
F2FS_INLINE_DENTRY = 0x04
F2FS_DATA_EXIST = 0x08
F2FS_INLINE_DOTS = 0x10
F2FS_EXTRA_ATTR = 0x20
DEFAULT_INLINE_XATTR_ADDRS = 50

# struct node_footer.
FOOTER = 4072
FOOTER_NID, FOOTER_INO, FOOTER_FLAG = 4072, 4076, 4080
FOOTER_CP_VER, FOOTER_NEXT = 4084, 4092
OFFSET_BIT_SHIFT = 3
ADDRS_PER_BLOCK = 1018         # direct node addresses
NIDS_PER_BLOCK = 1018          # indirect node nids
XATTR_NODE_OFFSET = (-1 & 0xFFFFFFFF) >> OFFSET_BIT_SHIFT    # 0x1FFFFFFF

N = NIDS_PER_BLOCK


def ofs_dnode(k):
    """Node offset of the inode's k-th direct node (i_nid[0], i_nid[1])."""
    return 1 + k


def ofs_ind(k):
    """Node offset of i_nid[2 + k] (k = 0, 1), an indirect node."""
    return 3 if k == 0 else 4 + N


def ofs_ind_child(k, i):
    """Node offset of direct node i below indirect node i_nid[2 + k]."""
    return ofs_ind(k) + 1 + i


OFS_DIND = 5 + 2 * N


def ofs_dind_ind(j):
    return 6 + 2 * N + j * (N + 1)


def ofs_dind_dnode(j, k):
    return 7 + 2 * N + j * (N + 1) + k


def write_footer(b, nid, ino, ofs, cp_ver=1, bits=0):
    struct.pack_into("<IIIQI", b, FOOTER, nid, ino, (ofs << OFFSET_BIT_SHIFT) | bits, cp_ver, 0)


def footer(b):
    """(nid, ino, ofs, flag bits, cp_ver) of a node block."""
    nid, ino, flag, cp_ver, _ = struct.unpack_from("<IIIQI", b, FOOTER)
    return nid, ino, flag >> OFFSET_BIT_SHIFT, flag & 7, cp_ver


def mark_superblock(b, sb_off=1024):
    """Write the format marker into version and init_version of one
    superblock copy that starts at byte `sb_off` of `b`."""
    for o in (SB_VERSION_OFF, SB_INIT_VERSION_OFF):
        b[sb_off + o:sb_off + o + 256] = MARKER.ljust(256, b"\0")


def is_lx_superblock(sb):
    """`sb` is the superblock struct (block 0 from byte 1024 on)."""
    return bytes(sb[SB_INIT_VERSION_OFF:SB_INIT_VERSION_OFF + len(MARKER)]) == MARKER


def checkpoint_reserve(meta_segs):
    """(rsvd, ovp) segment counts fsck.f2fs accepts: ckpt + sit + nat + ssa
    + rsvd >= F2FS_MIN_SEGMENT (9), both non-zero. `meta_segs` excludes the
    superblock segment."""
    rsvd = max(9 - meta_segs, 1)
    return rsvd, rsvd


# ---- dentry hash (lib/libf2fs.c, fs/f2fs/hash.c) -------------------------

_M = 0xFFFFFFFF
_DELTA = 0x9E3779B9


def _tea(buf, inp):
    s = 0
    b0, b1 = buf[0], buf[1]
    a, b, c, d = inp
    for _ in range(16):
        s = (s + _DELTA) & _M
        b0 = (b0 + ((((b1 << 4) & _M) + a) ^ (b1 + s) ^ ((b1 >> 5) + b))) & _M
        b1 = (b1 + ((((b0 << 4) & _M) + c) ^ (b0 + s) ^ ((b0 >> 5) + d))) & _M
    buf[0] = (buf[0] + b0) & _M
    buf[1] = (buf[1] + b1) & _M


def _str2hashbuf(msg, length, num):
    pad = length | (length << 8)
    pad = (pad | (pad << 16)) & _M
    out = []
    val = pad
    length = min(length, num * 4)
    for i in range(length):
        if i % 4 == 0:
            val = pad
        val = (msg[i] + (val << 8)) & _M
        if i % 4 == 3:
            out.append(val)
            val = pad
            num -= 1
    num -= 1
    if num >= 0:
        out.append(val)
    while num > 0:
        out.append(pad)
        num -= 1
    return out


def dentry_hash(name):
    """f2fs_dentry_hash of `name` (bytes): TEA over 16-byte chunks; "." and
    ".." hash to 0."""
    name = bytes(name)
    if name in (b".", b".."):
        return 0
    buf = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476]
    p, n = 0, len(name)
    while True:
        _tea(buf, _str2hashbuf(name[p:], n, 4))
        p += 16
        if n <= 16:
            break
        n -= 16
    return buf[0]


# ---- directory placement (fs/f2fs/dir.c, dir_level 0) ---------------------

NR_DENTRY_IN_BLOCK = 214
DENTRY_BITMAP = 27
DENTRY_ENTRIES = 30
DENTRY_NAMES = 30 + NR_DENTRY_IN_BLOCK * 11       # 2384
MAX_DIR_HASH_DEPTH = 63


def bucket_blocks(level):
    return 2 if level < MAX_DIR_HASH_DEPTH // 2 else 4


def dir_buckets(level, dir_level=0):
    if level + dir_level < MAX_DIR_HASH_DEPTH // 2:
        return 1 << (level + dir_level)
    return 1 << ((MAX_DIR_HASH_DEPTH // 2) - 1)


def dir_block_index(level, bucket, dir_level=0):
    bidx = sum(dir_buckets(i, dir_level) * bucket_blocks(i) for i in range(level))
    return bidx + bucket * bucket_blocks(level)


def block_level(pgofs, dir_level=0):
    """The hash level a directory block index belongs to (fsck.f2fs
    __get_current_level)."""
    bidx = 0
    for level in range(MAX_DIR_HASH_DEPTH):
        bidx += dir_buckets(level, dir_level) * bucket_blocks(level)
        if bidx > pgofs:
            return level
    return MAX_DIR_HASH_DEPTH - 1


def bucket_range(name_hash, pgofs, dir_level=0):
    """[first, end) block indices Linux searches for a name with this hash
    at the level that block `pgofs` belongs to."""
    level = block_level(pgofs, dir_level)
    b = dir_block_index(level, name_hash % dir_buckets(level, dir_level), dir_level)
    return b, b + bucket_blocks(level)


def place_dentries(entries, max_level=8):
    """Lay out (name bytes, ino, ftype) entries the way Linux's
    f2fs_add_regular_entry would: each name in the first block of its hash
    bucket, level by level, that has room. "." and ".." go first.

    Returns ({block index: 4 KiB bytearray}, i_current_depth); missing
    indices are holes."""
    blocks = {}
    used = {}
    depth = 1
    for name, ino, ftype in entries:
        slots = (len(name) + 7) // 8
        if not 0 < slots <= NR_DENTRY_IN_BLOCK:
            raise ValueError(f"bad directory entry name {name!r}")
        h = dentry_hash(name)
        done = False
        for level in range(max_level + 1):
            first = dir_block_index(level, h % dir_buckets(level))
            for bi in range(first, first + bucket_blocks(level)):
                at = used.get(bi, 0)
                if at + slots > NR_DENTRY_IN_BLOCK:
                    continue
                d = blocks.setdefault(bi, bytearray(4096))
                for k in range(at, at + slots):
                    d[k // 8] |= 1 << (k % 8)
                e = DENTRY_ENTRIES + at * 11
                struct.pack_into("<IIHB", d, e, h, ino, len(name), ftype)
                d[DENTRY_NAMES + at * 8:DENTRY_NAMES + at * 8 + len(name)] = name
                used[bi] = at + slots
                depth = max(depth, level + 1)
                done = True
                break
            if done:
                break
        if not done:
            raise ValueError(f"directory too large for hash level {max_level}: {name!r}")
    if not blocks:
        blocks[0] = bytearray(4096)
    return blocks, depth
