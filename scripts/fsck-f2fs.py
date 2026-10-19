#!/usr/bin/env python3
"""fsck-f2fs.py IMG [--churn DIR] — offline consistency check of a LeandrOS
f2fs volume (the layout servers/f2fs/src/lib.rs writes; upstream fsck.f2fs
rejects these images: no checkpoint CRC, a one-segment SSA).

Walks the namespace from the root inode through the NAT of the newest
checkpoint (the one the server mounts) and checks, for every inode, dnode,
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
        pack = (self.seg_cnt_ckpt // 2) * self.bps
        c0, c1 = self.blk(self.cp_blk), self.blk(self.cp_blk + pack)
        cp = c0 if r64(c0, 0) >= r64(c1, 0) else c1
        self.cp_ver = r64(cp, 0)
        self.cur_node = (r32(cp, 36), r16(cp, 68))
        self.cur_data = (r32(cp, 84), r16(cp, 116))
        self.next_nid = r32(cp, 152)
        self.flags = r32(cp, 132)
        self.main_end = self.main_blk + self.seg_cnt_main * self.bps

    def blk(self, n):
        self.f.seek(n * BLOCK)
        b = self.f.read(BLOCK)
        return b + bytes(BLOCK - len(b))

    def nat(self, nid):
        b = self.blk(self.nat_blk + nid // NAT_PER_BLK)
        o = (nid % NAT_PER_BLK) * NAT_ENTRY
        return r32(b, o + 5)

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
            if self.inodes[ino][1] != 2 + subdirs:
                self.warnings.append(f"dir {ino}: i_links {self.inodes[ino][1]} != {2 + subdirs}")
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


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 2
    img, churn, strict = argv[0], None, False
    i = 1
    while i < len(argv):
        if argv[i] == "--churn":
            churn = argv[i + 1]
            i += 2
        elif argv[i] == "--strict-leaks":
            strict = True
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
    for w in f.warnings[:10]:
        print("warning:", w)
    if f.leaked:
        print(f"{'ERROR' if strict else 'warning'}: {len(f.leaked)} blocks marked valid but unreachable "
              f"(first {f.leaked[:8]})")
        if strict:
            f.errors.append("leaks")
    print("fsck-f2fs: " + ("PASS" if not f.errors else f"FAIL ({len(f.errors)} errors)"))
    return 0 if not f.errors else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
