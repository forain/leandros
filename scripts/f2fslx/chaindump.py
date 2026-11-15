#!/usr/bin/env python3
"""chaindump.py <image> [--strict] [--quiet]

Host-only oracle for the f2fs fsync node chain (design f2fsnodelog, T1).
Picks the newest valid checkpoint (CRC), walks the warm-node chain exactly as
Linux's find_fsync_dnodes does (cp_ver stamp with the CP crc in the high half
when CP_CRC_RECOVERY is set, low 32 bits only when CP_NOCRC_RECOVERY is set,
META_POR bound, loop check), prints one line per chained node and checks the
design invariants:
  I1 footer fields sane       I3 FSYNC on every node, DENT only on an inode
  I4 no indirect node         I7 inode checksum (when the feature is on)
  I8 dnode addresses fresh (free in the CP's SIT) or the same slot as the home copy
  --strict adds the LeandrOS writer rules: I5 head at (S,0) of a segment empty in
  the CP SIT, chain stays in S in order with next=addr+1 and never uses slot bps-1;
  I6 no chain block is valid in the CP's SIT; I2 CP carries CRC_RECOVERY and not NOCRC.
Exit code: 0 no violation, 1 violations, 2 unusable image. Last line: "chaindump: ...".
"""
import struct, sys, zlib

MAGIC = 0xF2F52010
NODE_IND1, NODE_IND2, NODE_DIND, NODE_XATTR = 1, 2, 3, 0  # ofs values: see ofs_kind
FT = 4072


def crc32(d, seed=MAGIC):
    return (~zlib.crc32(d, ~seed & 0xFFFFFFFF)) & 0xFFFFFFFF


class Img:
    def __init__(s, path):
        s.f = open(path, "rb")
        sb = s.rd(1024, 2300)
        if struct.unpack_from("<I", sb, 0)[0] != MAGIC:
            raise SystemExit("chaindump: not an f2fs image (bad magic)")
        u = lambda o: struct.unpack_from("<I", sb, o)[0]
        s.bps = 1 << u(20)
        s.cp_blk, s.sit_blk, s.nat_blk, s.ssa_blk, s.main = u(76), u(80), u(84), u(88), u(92)
        s.seg_sit, s.seg_nat, s.seg_main = u(56), u(60), u(68)
        s.total = struct.unpack_from("<Q", sb, 36)[0]
        s.feat = u(2180)
        s.uuid = sb[108:124]

    def rd(s, off, n):
        s.f.seek(off)
        return s.f.read(n)

    def blk(s, a):
        b = s.rd(a * 4096, 4096)
        return b.ljust(4096, b"\0")


def pick_cp(im):
    best = None
    for pack in (0, 1):
        a = im.cp_blk + pack * im.bps
        h = im.blk(a)
        ver = struct.unpack_from("<Q", h, 0)[0]
        co = struct.unpack_from("<I", h, 164)[0]
        if co + 4 > 4096 or crc32(h[:co]) != struct.unpack_from("<I", h, co)[0]:
            continue
        total = struct.unpack_from("<I", h, 136)[0]
        if total < 2 or struct.unpack_from("<Q", im.blk(a + total - 1), 0)[0] != ver:
            continue
        if best is None or ver > best[0]:
            best = (ver, h, a, pack)
    return best


class Sit:
    """valid-bitmap view of the checkpoint's SIT (on-disk copy chosen by the CP bitmap, plus the journal)."""

    def __init__(s, im, cp, cp_addr):
        s.im, s.cache, s.jr = im, {}, {}
        s.bm = cp[192:192 + struct.unpack_from("<I", cp, 156)[0]]
        s.nat_bm = cp[192 + len(s.bm):192 + len(s.bm) + struct.unpack_from("<I", cp, 160)[0]]
        s.cpl = cp
        flags = struct.unpack_from("<I", cp, 132)[0]
        if flags & 4:  # compact summaries: journals live in the first summary block
            sb = im.blk(cp_addr + struct.unpack_from("<I", cp, 140)[0])
            s.nat_j, s.sit_j = sb[0:507], sb[507:1014]
            n = struct.unpack_from("<H", s.sit_j, 0)[0]
            for i in range(min(n, 6)):
                o = 2 + i * 78
                segno = struct.unpack_from("<I", s.sit_j, o)[0]
                s.jr[segno] = s.sit_j[o + 4:o + 78]
        else:
            s.nat_j = b"\0\0"

    def entry(s, segno):
        if segno in s.jr:
            return s.jr[segno]
        per = 4096 // 74
        bi, off = divmod(segno, per)
        if bi not in s.cache:
            a = s.im.sit_blk + bi
            if (s.bm[bi // 8] >> (7 - bi % 8)) & 1:
                a += s.im.seg_sit // 2 * s.im.bps
            s.cache[bi] = s.im.blk(a)
        return s.cache[bi][off * 74:off * 74 + 74]

    def valid(s, addr):
        if addr < s.im.main:
            return False
        segno, o = divmod(addr - s.im.main, s.im.bps)
        e = s.entry(segno)
        return (e[2 + o // 8] >> (7 - o % 8)) & 1 == 1

    def seg_valid_count(s, segno):
        return struct.unpack_from("<H", s.entry(segno), 0)[0] & 0x3FF

    def nat_addr(s, nid):
        """block_addr of nid in the checkpoint's NAT (journal first)."""
        n = struct.unpack_from("<H", s.nat_j, 0)[0]
        for i in range(min(n, 38)):
            o = 2 + i * 13
            if struct.unpack_from("<I", s.nat_j, o)[0] == nid:
                return struct.unpack_from("<I", s.nat_j, o + 9)[0]
        per = 4096 // 9
        bi, off = divmod(nid, per)
        a = s.im.nat_blk + bi
        if (s.nat_bm[bi // 8] >> (7 - bi % 8)) & 1:
            a += s.im.seg_nat // 2 * s.im.bps
        b = s.im.blk(a)
        return struct.unpack_from("<I", b, off * 9 + 5)[0]


def ofs_kind(ofs, nid, ino):
    """inode | dnode | xattr | indirect (node offsets per Linux fs/f2fs/node.h, 4 KiB / NIDS_PER_BLOCK 1018)."""
    if ofs == 0:
        return "inode"
    if ofs == 1:  # NODE_DIR1_BLOCK region begins; offsets 1,2 are direct (ADDRS_PER_INODE area)
        return "dnode"
    if ofs == 2:
        return "dnode"
    if ofs == 3:
        return "indirect"
    if 4 <= ofs <= 4 + 1017:  # inside the first indirect node's children: 3 is IND1, children 4..1021
        return "dnode"
    if ofs == 1022:  # IND2
        return "indirect"
    if 1023 <= ofs < 1023 + 1018 * 1019:
        return "dnode" if (ofs - 1023) % 1019 != 0 else "indirect"
    return "?"


def inode_addrs(b):
    """data addresses held in an inode copy; [] for inline data/dentry."""
    inl = b[3]
    if inl & 0x06:
        return []
    n, base = 923, 360
    if inl & 0x20:  # F2FS_EXTRA_ATTR
        ex = struct.unpack_from("<H", b, 360)[0]
        base += ex
        n -= ex // 4
    if inl & 0x01:  # inline xattr
        xs = struct.unpack_from("<H", b, 362)[0] if (inl & 0x20 and inl & 0x40) else 50
        n -= xs
    return [struct.unpack_from("<I", b, base + 4 * i)[0] for i in range(max(n, 0))]


def main(argv):
    path = [a for a in argv if not a.startswith("--")]
    strict, quiet = "--strict" in argv, "--quiet" in argv
    if not path:
        print(__doc__); return 2
    im = Img(path[0])
    cp = pick_cp(im)
    if not cp:
        print("chaindump: no valid checkpoint"); return 2
    ver, h, cpa, pack = cp
    flags = struct.unpack_from("<I", h, 132)[0]
    co = struct.unpack_from("<I", h, 164)[0]
    crc = struct.unpack_from("<I", h, co)[0]
    seg = struct.unpack_from("<I", h, 36 + 4)[0]
    off = struct.unpack_from("<H", h, 68 + 2)[0]
    bad = []
    say = (lambda *a: None) if quiet else print
    say(f"cp pack={pack} ver={ver} flags={flags:#x} (umount={flags & 1} crcrec={bool(flags & 0x40)} nocrc={bool(flags & 0x200)}) "
        f"warm head seg={seg} off={off} bps={im.bps}")
    sit = Sit(im, h, cpa)
    want = ver | (crc << 32) if flags & 0x40 and not flags & 0x200 else ver
    if strict:
        if not flags & 0x40: bad.append("I2: CP lacks CRC_RECOVERY 0x40")
        if flags & 0x200: bad.append("I2: CP carries NOCRC 0x200")
        if off != 0: bad.append(f"I5: head offset {off} != 0")
        if sit.seg_valid_count(seg) != 0: bad.append(f"I5: head segment {seg} not empty in the CP SIT")
    nodes, addr, total_free, loop = [], im.main + seg * im.bps + off, im.total, 0
    mask = 0xFFFFFFFF if flags & 0x40 and flags & 0x200 else 0xFFFFFFFFFFFFFFFF
    while addr >= im.main and addr < im.main + im.seg_main * im.bps:  # META_POR bound
        b = im.blk(addr)
        nid, ino, fl = struct.unpack_from("<III", b, FT)
        cv, nxt = struct.unpack_from("<QI", b, FT + 12)
        if (cv & mask) != (want & mask):
            break
        nodes.append((addr, nid, ino, fl, b, nxt))
        loop += 1
        if loop >= total_free or addr == nxt:
            bad.append(f"loop in chain at {addr}"); break
        addr = nxt
    say(f"chain: {len(nodes)} node(s) stamped for this checkpoint")
    first_seg = nodes and (nodes[0][0] - im.main) // im.bps
    nrec = 0
    for k, (a, nid, ino, fl, b, nxt) in enumerate(nodes):
        ofs = fl >> 3
        kind = ofs_kind(ofs, nid, ino)
        cold, fs, dent = fl & 1, bool(fl & 2), bool(fl & 4)
        nrec += fs or dent
        say(f"  {a:8} nid={nid:<6} ino={ino:<6} ofs={ofs:<5} {kind:8} {'C' if cold else '-'}{'F' if fs else '-'}{'D' if dent else '-'} next={nxt}")
        # I1/I3/I4
        if nid == 0 or ino == 0: bad.append(f"I1: node {a} has nid/ino 0")
        if (ofs == 0) != (nid == ino): bad.append(f"I4: node {a} ofs={ofs} nid={nid} ino={ino} inconsistent")
        if kind == "indirect": bad.append(f"I4: indirect node chained at {a} (ofs {ofs})")
        if kind == "?": bad.append(f"I4: node {a} has unknown ofs {ofs}")
        if strict and not fs: bad.append(f"I3: node {a} lacks FSYNC")
        if dent and nid != ino: bad.append(f"I3: DENT on non-inode {a}")
        # I7: Linux f2fs_inode_chksum over the whole block (footer included) with the field zeroed
        if kind == "inode" and im.feat & 0x800 and b[3] & 0x20:
            o = 368
            got = struct.unpack_from("<I", b, o)[0]
            c = crc32(im.uuid, 0xFFFFFFFF)
            c = crc32(struct.pack("<I", ino), c)
            c = crc32(b[68:72], c)
            c = crc32(b[:o] + b"\0\0\0\0" + b[o + 4:], c) if False else crc32(b[o + 4:], crc32(b"\0\0\0\0", crc32(b[:o], c)))
            if c != got: bad.append(f"I7: inode checksum of node {a} is {got:#x}, expected {c:#x}")
        # I8 (data addresses of dnodes/inodes)
        if kind in ("dnode", "inode"):
            addrs = inode_addrs(b) if kind == "inode" else [struct.unpack_from("<I", b, 4 * i)[0] for i in range(1018)]
            home = im.blk(sit.nat_addr(nid)) if sit.nat_addr(nid) else None
            hold = None
            if home is not None:
                hold = inode_addrs(home) if kind == "inode" else [struct.unpack_from("<I", home, 4 * i)[0] for i in range(1018)]
            for i, d in enumerate(addrs):
                if d in (0, 0xFFFFFFFF) or d < im.main or d >= im.main + im.seg_main * im.bps:
                    continue
                same = hold is not None and i < len(hold) and hold[i] == d
                if sit.valid(d) and not same:
                    bad.append(f"I8: node {a} slot {i} -> {d} is valid in the CP SIT and not the home slot")
        # I5/I6
        if strict:
            if (a - im.main) // im.bps != first_seg: bad.append(f"I5: node {a} left segment {first_seg}")
            if a != nodes[0][0] + k: bad.append(f"I5: node {a} not at S+{k}")
            if (a - im.main) % im.bps == im.bps - 1: bad.append(f"I5: slot bps-1 used at {a}")
            if nxt != a + 1: bad.append(f"I5: node {a} next={nxt} != {a + 1}")
            if sit.valid(a): bad.append(f"I6: chain block {a} is valid in the CP SIT")
    say(f"fsync/dent marked nodes: {nrec}")
    for m in bad:
        print("VIOLATION", m)
    print(f"chaindump: ver={ver} chain={len(nodes)} marked={nrec} violations={len(bad)} cp_umount={flags & 1}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
