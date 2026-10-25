#!/usr/bin/env python3
"""fsck-f2fs.py IMG [--churn DIR] [--strict-leaks] [--strict-links] — offline
consistency check of a LeandrOS f2fs volume (the layout
servers/f2fs/src/lib.rs writes; upstream fsck.f2fs rejects these images: a
one-segment SSA, a single NAT/SIT copy, our own checkpoint journal).

Picks the checkpoint the server mounts (lane f2fsync: the newest pack whose
commit block passes its CRC, journal replayed on top of the home blocks, as
servers/f2fs/src/durable.rs does; roll-forward fsync records are not applied,
so this checks the committed checkpoint), walks the namespace from the root
inode through its NAT and checks, for every inode, dnode,
indirect node, xattr node and data block reachable from it:

  - it lies inside the main area,
  - no other reachable block uses the same address,
  - the SIT marks it valid (else the allocator may hand it out again: data
    loss),
  - every segment's vblocks equals the population of its valid map,
  - every dentry names an inode the NAT maps, and directory link counts.

Blocks the SIT marks valid that nothing reaches are reported as leaked (an
error with --strict-leaks). With --churn DIR (a volume path such as
/gc), DIR/MANIFEST written by `f2fstest churn` is read and every file it
lists is checked for size and content, word by word.

Exit 0 iff no errors.
"""
import struct
import sys
import zlib

BLOCK = 4096
NAT_ENTRY = 9
NAT_PER_BLK = BLOCK // NAT_ENTRY
SIT_ENTRY = 74
SIT_PER_BLK = BLOCK // SIT_ENTRY
NODE_FOOTER = 4076
ADDRS_PER_DNODE = NODE_FOOTER // 4      # 1019
INODE_UNION = 364
INODE_NIDS = 4056
S_IFMT, S_IFDIR = 0o170000, 0o040000


CPJ_MAGIC, CPJ_MAGIC_OFF = 0x314A504C, 3072
CPJ_INCOMPAT_OFF, CPJ_INCOMPAT_KNOWN = 3360, 1   # bit 0 = roll-forward records may be present
# Orphan list (lane f2fsorphan): u32 count, then nids of inodes unlinked while
# open at checkpoint time; the mount reaps them. Old kernels ignore it.
CPJ_ORPHAN_CNT_OFF, CPJ_ORPHAN_OFF, CPJ_ORPHAN_MAX = 3364, 3368, (4092 - 3368) // 4


def cp_crc_ok(b):
    if r32(b, 164) == 4092:
        return linux_crc(b[:4092]) == r32(b, 4092)
    return zlib.crc32(b[:4092]) == r32(b, 4092)


def cp_legacy(b):
    """A commit block from before lane f2fscrc: checksum_offset 0, zlib CRC,
    journal and records may use the whole pack."""
    return r32(b, 164) != 4092


F2FS_MAGIC = 0xF2F52010
LINUX_TAIL = 7      # 6 curseg summaries + the cp footer at the end of the pack
LINUX_TAIL_V1 = 6   # lane f2fscrc packs with cp_pack_total_block_count 1


def cp_tail(b):
    """Blocks at the end of the pack the journal and records stay out of."""
    if cp_legacy(b):
        return 0
    return LINUX_TAIL_V1 if r32(b, 136) <= 2 else LINUX_TAIL


def linux_crc(data):
    """Linux f2fs_crc32: crc32_le seeded with the magic, no inversion."""
    return (~zlib.crc32(bytes(data), (~F2FS_MAGIC) & 0xFFFFFFFF)) & 0xFFFFFFFF


def linux_crc_bitwise(data):
    """The same, bit by bit (independent of zlib), for the guard."""
    c = F2FS_MAGIC
    for x in bytes(data):
        c ^= x
        for _ in range(8):
            c = (c >> 1) ^ (0xEDB88320 if c & 1 else 0)
    return c


def r16(b, o): return struct.unpack_from("<H", b, o)[0]
def r32(b, o): return struct.unpack_from("<I", b, o)[0]
def r64(b, o): return struct.unpack_from("<Q", b, o)[0]


def mix(z):
    m = (1 << 64) - 1
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & m
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & m
    return z ^ (z >> 31)


def pattern(key, off, n):
    m = (1 << 64) - 1
    base = (key * 0x9E3779B97F4A7C15) & m
    w0 = off // 8
    return b"".join(struct.pack("<Q", mix((base + w0 + i) & m)) for i in range((n + 7) // 8))[:n]


class Vol:
    def __init__(self, path):
        self.f = open(path, "rb")
        self.overlay = {}
        sb = self.blk(0)[1024:]
        if r32(sb, 0) != 0xF2F52010:
            raise SystemExit("not an f2fs volume")
        self.bps = 1 << r32(sb, 20)
        self.block_count = r64(sb, 36)
        self.seg_cnt_ckpt = r32(sb, 52)
        self.seg_cnt_nat = r32(sb, 60)
        self.seg_cnt_main = r32(sb, 68)
        self.cp_blk = r32(sb, 76)
        self.sit_blk = r32(sb, 80)
        self.nat_blk = r32(sb, 84)
        self.main_blk = r32(sb, 92)
        self.root = r32(sb, 96)
        self.overlay = {}
        pack = (self.seg_cnt_ckpt // 2) * self.bps
        self.pack = pack
        c0, c1 = self.blk(self.cp_blk), self.blk(self.cp_blk + pack)
        j0, j1 = r32(c0, CPJ_MAGIC_OFF) == CPJ_MAGIC, r32(c1, CPJ_MAGIC_OFF) == CPJ_MAGIC
        ok0 = cp_crc_ok(c0) if j0 else True
        ok1 = cp_crc_ok(c1) if j1 else True
        if ok0 and ok1:
            pick1 = r64(c1, 0) > r64(c0, 0)
        elif ok0 or ok1:
            pick1 = ok1
        else:
            print("warning: both checkpoint packs fail their checks")
            pick1 = r64(c1, 0) > r64(c0, 0)
        jn = self.journal(c1 if pick1 else c0, self.cp_blk + (pack if pick1 else 0))
        if jn is None and (ok0 if pick1 else ok1):
            print("warning: the newest checkpoint's journal is corrupt; using the other pack")
            pick1 = not pick1
            jn = self.journal(c1 if pick1 else c0, self.cp_blk + (pack if pick1 else 0))
        cp = c1 if pick1 else c0
        self.journal_blocks = 0
        if jn:
            self.journal_blocks = len(jn)
            self.overlay = jn
        self.cp_ver = r64(cp, 0)
        self.cp_raw = cp
        self.cp_addr = self.cp_blk + (pack if pick1 else 0)
        self.sb_raw = sb
        self.cur_node = (r32(cp, 36), r16(cp, 68))
        self.cur_data = (r32(cp, 84), r16(cp, 116))
        if r32(cp, 164) == 4092:
            # Linux-CRC checkpoint (lane f2fscrc3): the kernel's own log heads
            # live at 3348; the standard fields hold heads Linux accepts.
            self.cur_data = (r32(cp, 3348), r16(cp, 3356))
            self.cur_node = (r32(cp, 3352), r16(cp, 3358))
        self.next_nid = r32(cp, 152)
        self.flags = r32(cp, 132)
        self.incompat = r32(cp, CPJ_INCOMPAT_OFF) if r32(cp, CPJ_MAGIC_OFF) == CPJ_MAGIC else 0
        n_orph = min(r32(cp, CPJ_ORPHAN_CNT_OFF), CPJ_ORPHAN_MAX) if r32(cp, CPJ_MAGIC_OFF) == CPJ_MAGIC else 0
        self.orphans = [r32(cp, CPJ_ORPHAN_OFF + 4 * i) for i in range(n_orph)]
        self.main_end = self.main_blk + self.seg_cnt_main * self.bps

    def journal(self, cpb, base):
        """{home: image} of the pack's journal, {} if it has none, None if
        its CRC fails."""
        if r32(cpb, CPJ_MAGIC_OFF) != CPJ_MAGIC:
            return {}
        total, j, nx = r32(cpb, 3076), r32(cpb, 3080), r32(cpb, 3088)
        ext = [r32(cpb, 3092 + 4 * q) for q in range(min(nx, 64))]
        cap = self.pack - 1 - cp_tail(cpb)
        h = (j + 1019) // 1020
        if h + j != total or total > cap + 512 * len(ext):
            return None
        def addr(k):
            if k < cap:
                return base + 1 + k
            e = k - cap
            return self.main_blk + ext[e // 512] * self.bps + e % 512
        crc = 0
        homes, imgs = [], []
        for k in range(total):
            b = self.raw(addr(k))
            crc = zlib.crc32(b, crc)
            if k < h:
                if r32(b, 0) != 0x58494A4C:
                    return None
                homes += [r32(b, 16 + 4 * q) for q in range(min(r32(b, 4), 1020))]
            else:
                imgs.append(b)
        if crc != r32(cpb, 3084) or len(homes) != j:
            return None
        return dict(zip(homes, imgs))

    def raw(self, n):
        self.f.seek(n * BLOCK)
        b = self.f.read(BLOCK)
        return b + bytes(BLOCK - len(b))

    def blk(self, n):
        if n in self.overlay:
            return self.overlay[n]
        self.f.seek(n * BLOCK)
        b = self.f.read(BLOCK)
        return b + bytes(BLOCK - len(b))

    def nat(self, nid):
        b = self.blk(self.nat_blk + nid // NAT_PER_BLK)
        o = (nid % NAT_PER_BLK) * NAT_ENTRY
        return r32(b, o + 5)

    def sit_raw(self, seg):
        b = self.blk(self.sit_blk + seg // SIT_PER_BLK)
        o = (seg % SIT_PER_BLK) * SIT_ENTRY
        return b[o:o + SIT_ENTRY]

    def sit(self, seg):
        b = self.blk(self.sit_blk + seg // SIT_PER_BLK)
        o = (seg % SIT_PER_BLK) * SIT_ENTRY
        return r16(b, o) & 0x3FF, b[o + 2:o + 66]


def inode_addrs(ib):
    base = INODE_UNION
    if ib[3] & 0x20:
        base = INODE_UNION + min(r16(ib, INODE_UNION) // 4, 64) * 4
    n = (INODE_NIDS - base) // 4
    return [r32(ib, base + 4 * i) for i in range(n)]


class Fsck:
    def __init__(self, v):
        self.v = v
        self.errors = []
        self.warnings = []
        self.owner = {}          # blkaddr -> description
        self.valid = {}          # seg -> valid map bytes
        self.inodes = {}         # ino -> (mode, links, size, data block list)
        self.dir_children = {}
        self.link_errors = []
        self.unknown_links = 0

    def err(self, m):
        if len(self.errors) < 50:
            print("ERROR:", m)
        self.errors.append(m)

    def is_valid(self, a):
        off = a - self.v.main_blk
        seg, b = divmod(off, self.v.bps)
        m = self.valid[seg]
        return (m[b // 8] >> (b % 8)) & 1

    def ref(self, a, what):
        v = self.v
        if a < v.main_blk or a >= v.main_end:
            self.err(f"{what}: block {a} outside the main area")
            return False
        if a in self.owner:
            self.err(f"{what}: block {a} also used by {self.owner[a]}")
            return False
        self.owner[a] = what
        if not self.is_valid(a):
            self.err(f"{what}: block {a} is in use but the SIT marks it free")
        return True

    def node(self, nid, what):
        a = self.v.nat(nid)
        if a == 0:
            self.err(f"{what}: nid {nid} has no NAT entry")
            return None
        if not self.ref(a, f"{what} (nid {nid})"):
            return None
        b = self.v.blk(a)
        if r32(b, NODE_FOOTER) != nid:
            self.err(f"{what}: node block {a} footer nid {r32(b, NODE_FOOTER)} != {nid}")
        return b

    def file_blocks(self, ino, ib):
        """Logical-order data block addresses (0 = hole) of inode `ino`."""
        out = []
        for a in inode_addrs(ib):
            out.append(a)
        def dnode(nid, tag):
            if nid == 0:
                out.extend([0] * ADDRS_PER_DNODE)
                return
            b = self.node(nid, f"ino {ino} {tag}")
            if b is None:
                out.extend([0] * ADDRS_PER_DNODE)
                return
            out.extend(r32(b, 4 * i) for i in range(ADDRS_PER_DNODE))
        def indirect(nid, tag):
            if nid == 0:
                return False
            b = self.node(nid, f"ino {ino} {tag}")
            if b is None:
                return False
            kids = [r32(b, 4 * i) for i in range(ADDRS_PER_DNODE)]
            last = max([i for i, k in enumerate(kids) if k] or [-1])
            for i in range(last + 1):
                dnode(kids[i], f"{tag}[{i}] dnode")
            return True
        nids = [r32(ib, INODE_NIDS + 4 * k) for k in range(5)]
        if any(nids):
            dnode(nids[0], "dnode0")
            dnode(nids[1], "dnode1")
            if nids[2] or nids[3] or nids[4]:
                if not indirect(nids[2], "ind0"):
                    out.extend([0] * ADDRS_PER_DNODE * ADDRS_PER_DNODE)
                if nids[3] or nids[4]:
                    if not indirect(nids[3], "ind1"):
                        out.extend([0] * ADDRS_PER_DNODE * ADDRS_PER_DNODE)
                    if nids[4]:
                        b = self.node(nids[4], f"ino {ino} dind")
                        if b is not None:
                            kids = [r32(b, 4 * i) for i in range(ADDRS_PER_DNODE)]
                            last = max([i for i, k in enumerate(kids) if k] or [-1])
                            for i in range(last + 1):
                                if not indirect(kids[i], f"dind[{i}]"):
                                    out.extend([0] * ADDRS_PER_DNODE * ADDRS_PER_DNODE)
        for i, a in enumerate(out):
            if a:
                self.ref(a, f"ino {ino} data[{i}]")
        return out

    def run(self):
        v = self.v
        for seg in range(v.seg_cnt_main):
            vb, m = v.sit(seg)
            self.valid[seg] = m
            pop = sum(bin(x).count("1") for x in m)
            if vb != pop:
                self.err(f"segment {seg}: vblocks {vb} != valid-map population {pop}")
        todo = [v.root]
        # Listed orphans (still an inode block of that nid with no links) own
        # their blocks until the next mount reaps them: not leaks.
        self.orphans = []
        for o in v.orphans:
            a = v.nat(o)
            if o > v.root and a and self.is_valid(a):
                b = v.blk(a)
                if r32(b, 4076) == o and r32(b, 4080) == o and r32(b, 12) == 0 and r16(b, 0):
                    self.orphans.append(o)
        todo += self.orphans
        seen = set()
        while todo:
            ino = todo.pop()
            if ino in seen:
                continue
            seen.add(ino)
            ib = self.node(ino, f"inode {ino}")
            if ib is None:
                continue
            xnid = r32(ib, 24)
            if xnid:
                self.node(xnid, f"ino {ino} xattr")
            mode, links, size = r16(ib, 0), r32(ib, 12), r64(ib, 16)
            blocks = self.file_blocks(ino, ib)
            self.inodes[ino] = (mode, links, size, blocks)
            if mode & S_IFMT == S_IFDIR:
                kids = []
                nblk = (size + BLOCK - 1) // BLOCK
                for a in blocks[:nblk]:
                    if a == 0:
                        continue
                    d = v.blk(a)
                    slot = 0
                    while slot < 214:
                        if not (d[slot // 8] >> (slot % 8)) & 1:
                            slot += 1
                            continue
                        e = 30 + slot * 11
                        child, nlen, ft = r32(d, e + 4), r16(d, e + 8), d[e + 10]
                        name = d[2384 + slot * 8:2384 + slot * 8 + nlen]
                        kids.append((name, child, ft))
                        slot += max(1, (nlen + 7) // 8)
                self.dir_children[ino] = kids
                for name, child, ft in kids:
                    if name in (b".", b".."):
                        continue
                    if v.nat(child) == 0:
                        self.err(f"dir {ino}: entry {name!r} names inode {child} with no NAT entry")
                        continue
                    todo.append(child)
        # Link counts of directories: 2 + subdirectories.
        for ino, kids in self.dir_children.items():
            subdirs = sum(1 for n, c, ft in kids if ft == 4 and n not in (b".", b".."))
            links = self.inodes[ino][1]
            if links == 1:
                self.unknown_links += 1     # pre-f2fsync mkdir: "unknown"
            elif links != 2 + subdirs:
                self.link_errors.append(f"dir {ino}: i_links {links} != {2 + subdirs}")
        valid = sum(sum(bin(x).count("1") for x in m) for m in self.valid.values())
        self.leaked = []
        for seg, m in self.valid.items():
            for b in range(v.bps):
                if (m[b // 8] >> (b % 8)) & 1:
                    a = v.main_blk + seg * v.bps + b
                    if a not in self.owner:
                        self.leaked.append(a)
        self.valid_total = valid

    def lookup(self, path):
        ino = self.v.root
        for comp in [c for c in path.split("/") if c]:
            kids = self.dir_children.get(ino, [])
            hit = [c for n, c, ft in kids if n == comp.encode()]
            if not hit:
                return None
            ino = hit[0]
        return ino

    def read_file(self, ino):
        mode, links, size, blocks = self.inodes[ino]
        data = bytearray()
        for i in range((size + BLOCK - 1) // BLOCK):
            a = blocks[i] if i < len(blocks) else 0
            data += self.v.blk(a) if a else bytes(BLOCK)
        return bytes(data[:size])

    def churn(self, d):
        dino = self.lookup(d)
        if dino is None:
            self.err(f"--churn: {d} not found")
            return
        kids = {n: c for n, c, ft in self.dir_children.get(dino, [])}
        if b"MANIFEST" not in kids:
            self.err(f"--churn: {d}/MANIFEST not found")
            return
        man = self.read_file(kids[b"MANIFEST"]).decode()
        nfiles = nbytes = 0
        for line in man.split("\n"):
            if not line.strip():
                continue
            slot, gen, size = (int(x) for x in line.split())
            name = f"f{slot}".encode()
            if name not in kids:
                self.err(f"--churn: {name!r} listed but missing")
                continue
            data = self.read_file(kids[name])
            if len(data) != size:
                self.err(f"--churn: {name!r} size {len(data)} != {size}")
                continue
            want = pattern((slot << 32) | gen, 0, size)
            if data != want:
                bad = next(i for i in range(size) if data[i] != want[i])
                self.err(f"--churn: {name!r} content differs from byte {bad}")
                continue
            nfiles += 1
            nbytes += size
        print(f"churn data: {nfiles} files, {nbytes >> 10} KiB match their pattern")


def linux_check(v):
    """What Linux (6.12 f2fs_get_valid_checkpoint, sanity_check_ckpt,
    restore_curseg_summaries, build_sit_entries, sanity_check_curseg) checks
    of the checkpoint it would mount. [] if it would accept it."""
    cp, sb, bps, main = v.cp_raw, v.sb_raw, v.bps, v.seg_cnt_main
    bad = []
    off = r32(cp, 164)
    if off != 4092:
        return [f"invalid crc_offset: {off}"]
    if linux_crc_bitwise(cp[:4092]) != r32(cp, 4092):
        bad.append("invalid crc value")
    total = r32(cp, 136)
    if total <= 2 or total > bps:  # F2FS_CP_PACKS < total <= blocks_per_seg
        bad.append(f"invalid cp_pack_total_block_count: {total}")
    else:
        foot = v.raw(v.cp_addr + total - 1)
        if r64(foot, 0) != r64(cp, 0) or r32(foot, 164) != 4092 or linux_crc(foot[:4092]) != r32(foot, 4092):
            bad.append("cp footer does not match the header")
    segs, ssa, sit, nat, ckpt = r32(sb, 48), r32(sb, 64), r32(sb, 56), r32(sb, 60), r32(sb, 52)
    rsvd, ovp = r32(cp, 24), r32(cp, 28)
    fsmeta = ckpt + sit + nat + rsvd + ssa
    if fsmeta >= segs or fsmeta < 8 or ovp == 0 or rsvd == 0:
        bad.append(f"Wrong layout (fsmeta {fsmeta}, ovp {ovp}, rsvd {rsvd})")
    user, valid = r64(cp, 8), r64(cp, 16)
    if user == 0 or user >= main * bps:
        bad.append(f"Wrong user_block_count: {user}")
    if valid > user:
        bad.append(f"Wrong valid_user_blocks: {valid}, user_block_count: {user}")
    node = [(r32(cp, 36 + 4 * i), r16(cp, 68 + 2 * i)) for i in range(3)]
    data = [(r32(cp, 84 + 4 * i), r16(cp, 116 + 2 * i)) for i in range(3)]
    for name, logs in (("Node", node), ("Data", data)):
        for s_, o in logs:
            if s_ >= main or o >= bps:
                bad.append(f"{name} curseg out of range: ({s_}, {o})")
    segnos = [s_ for s_, _ in node + data]
    if len(set(segnos)) != 6:
        bad.append(f"cursegs share a segment: {segnos}")
    if r32(cp, 156) != ((sit // 2) * bps) // 8 or r32(cp, 160) != ((nat // 2) * bps) // 8:
        bad.append("Wrong bitmap size")
    # Linux 7.2 sanity_check_ckpt: the SIT version bitmap must cover every
    # SIT block, which a single (unmirrored) SIT segment cannot give.
    sit_blk_cnt = -(-main // 55)
    if r32(cp, 156) * 8 < sit_blk_cnt:
        bad.append(f"Wrong bitmap size: sit: {r32(cp, 156)}, sit_blk_cnt:{sit_blk_cnt}")
    if 192 + r32(cp, 156) + r32(cp, 160) > 3072:
        bad.append("version bitmaps overlap the LPJ1 extension")
    ss = r32(cp, 140)
    if ss < 1 or ss > bps - 1 - 6:
        bad.append(f"Wrong cp_pack_start_sum: {ss}")
    flags = r32(cp, 132)
    if flags & 0x16:
        bad.append(f"unsupported ckpt_flags {flags:#x} (orphan/compact/error)")
    base = 6 if flags & 0x401 else 3
    if 0 < total <= bps:
        for t, lim, name in ((0, 38, "nat"), (2, 6, "sit")):
            b = v.raw(v.cp_addr + total - (base + 1) + t)
            if r16(b, 3584) > lim:
                bad.append(f"invalid journal entries ({name} {r16(b, 3584)})")
    nodes = tot = 0
    for seg in range(main):
        e = v.sit_raw(seg)
        vb, ty = r16(e, 0) & 0x3FF, r16(e, 0) >> 10
        tot += vb
        if 3 <= ty <= 5:
            nodes += vb
    if nodes != r32(cp, 144):
        bad.append(f"SIT is corrupted node# {nodes} vs {r32(cp, 144)}")
    if tot > valid:
        bad.append(f"SIT is corrupted data# {tot} vs {valid}")
    for t, (s_, o) in enumerate(data + node):
        at = cp[176 + t]
        if at not in (0, 1) or s_ >= main or o >= bps:
            bad.append(f"curseg {t}: alloc_type {at}") if at not in (0, 1) else None
            continue
        vm = v.sit_raw(s_)[2:66]
        bit = lambda k: vm[k // 8] >> (7 - k % 8) & 1
        if bit(o) or (at == 0 and any(bit(k) for k in range(o + 1, bps))):
            bad.append(f"curseg {t} ({s_}, {o}) points at a valid block")
    return bad


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 2
    img, churn, strict, strict_links = argv[0], None, False, False
    linux = False
    i = 1
    while i < len(argv):
        if argv[i] == "--churn":
            churn = argv[i + 1]
            i += 2
        elif argv[i] == "--linux":
            linux = True
        elif argv[i] == "--strict-leaks":
            strict = True
            i += 1
        elif argv[i] == "--strict-links":
            strict_links = True
            i += 1
        else:
            print(f"unknown argument {argv[i]}")
            return 2
    v = Vol(img)
    f = Fsck(v)
    f.run()
    if churn:
        f.churn(churn)
    print(f"checkpoint ver {v.cp_ver} ({'clean' if v.flags & 1 else 'NOT clean'}), "
          f"{len(f.inodes)} inodes, {len(f.owner)} blocks in use, SIT valid {f.valid_total}, "
          f"leaked {len(f.leaked)}, next nid {v.next_nid}")
    if v.incompat:
        unk = v.incompat & ~CPJ_INCOMPAT_KNOWN
        print(f"feature mask {v.incompat:#x}: known bits {v.incompat & CPJ_INCOMPAT_KNOWN:#x}, "
              f"unknown bits {unk:#x}" + (" (a kernel mounts this volume read-only)" if unk else ""))
    if v.orphans:
        print(f"orphan list: {len(v.orphans)} nid(s), {len(f.orphans)} still unlinked inodes (reaped at mount)")
    if v.journal_blocks:
        print(f"checkpoint journal: {v.journal_blocks} block(s) read over the home blocks")
    for w in f.warnings[:10]:
        print("warning:", w)
    for w in f.link_errors[:10]:
        print("ERROR:" if strict_links else "warning:", w)
    if f.unknown_links:
        print(f"note: {f.unknown_links} directories carry the 'unknown' link count 1")
    if strict_links and f.link_errors:
        f.errors.append("links")
    if f.leaked:
        print(f"{'ERROR' if strict else 'warning'}: {len(f.leaked)} blocks marked valid but unreachable "
              f"(first {f.leaked[:8]})")
        if strict:
            f.errors.append("leaks")
    lbad = linux_check(v)
    for w in lbad[:10]:
        print(("ERROR" if linux else "warning") + ": linux:", w)
    print("linux checkpoint: " + ("ok" if not lbad else f"REJECTED ({len(lbad)})"))
    if linux and lbad:
        f.errors.append("linux")
    print("fsck-f2fs: " + ("PASS" if not f.errors else f"FAIL ({len(f.errors)} errors)"))
    return 0 if not f.errors else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
