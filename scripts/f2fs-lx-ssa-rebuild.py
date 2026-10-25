#!/usr/bin/env python3
"""f2fs-lx-ssa-rebuild.py IMG [--check]

Offline repair of an lx (Linux-layout) LeandrOS f2fs image written by a kernel
that did not yet maintain the SSA (lxnode1 images and early lxnode2 ones): from
the NAT and the node tree of the newest committed checkpoint it rebuilds

  - the SSA owner entries (nid, version, ofs_in_node) and segment footers,
  - the six curseg summary blocks in the checkpoint pack tail,
  - the SIT segment types (3 = HOT_NODE for node segments, 0 = HOT_DATA for
    data segments; the six cursegs get their own type 0..5),
  - NAT entries for inode 1 and 2,
  - cp free_segment_count and valid_inode_count, re-sealed with the Linux CRC
    (both copies of the pack).

It refuses (exit 2) on a segment that mixes node and data blocks (recreate the
volume: mkfs now keeps them apart), on a legacy volume, and on a checkpoint that
carries a journal (mount and unmount the volume cleanly first). --check only
reports what would be done. Exit 0 on success, 1 on a tree error.
"""
import importlib.util
import os
import struct
import sys

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, here)
import f2fslx  # noqa: E402

spec = importlib.util.spec_from_file_location("fsck_f2fs", os.path.join(here, "fsck-f2fs.py"))
fsck = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fsck)

B = 4096


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 2
    img, check = argv[0], "--check" in argv[1:]
    v = fsck.Vol(img)
    if not v.lx:
        print("not an lx volume (legacy images keep their own layout)")
        return 2
    if v.journal_blocks:
        print("the checkpoint carries a journal: mount and unmount the volume cleanly first")
        return 2
    f = fsck.Fsck(v)
    f.run()
    tree = [m for m in f.errors if not m.startswith("lx: ")]
    if tree:
        print(f"{len(tree)} tree errors, not rebuilding")
        return 1
    cp, bps = v.cp_raw, v.bps
    ssa_blk, nmain = fsck.r32(v.sb_raw, 88), v.seg_cnt_main
    cur = [fsck.r32(cp, 84 + 4 * i) for i in range(3)] + [fsck.r32(cp, 36 + 4 * i) for i in range(3)]
    total, ssum = fsck.r32(cp, 136), fsck.r32(cp, 140)
    sums = {}
    for a, ent in f.exp.items():
        sums.setdefault((a - v.main_blk) // bps, {})[(a - v.main_blk) % bps] = ent
    mixed = [s for s, ents in sums.items() if len({e[2] for e in ents.values()}) > 1]
    if mixed:
        print(f"{len(mixed)} segment(s) mix node and data blocks (first {mixed[:5]}): recreate the volume")
        return 2
    out = open(img, "rb" if check else "r+b")

    def wr(n, b):
        if not check:
            out.seek(n * B)
            out.write(bytes(b))
    types = {}
    for seg in range(nmain):
        ents = sums.get(seg, {})
        isnode = any(e[2] for e in ents.values())
        t = cur.index(seg) if seg in cur else (3 if isnode else 0)
        types[seg] = t
        if not ents and seg not in cur:
            continue
        sb = bytearray(B)
        for off, (nid, ofs, node) in ents.items():
            ver = 0 if node else v.blk(v.nat_blk + nid // fsck.NAT_PER_BLK)[(nid % fsck.NAT_PER_BLK) * fsck.NAT_ENTRY]
            struct.pack_into("<IBH", sb, off * 7, nid, ver, ofs)
        sb[4091] = 1 if t >= 3 else 0
        wr(ssa_blk + seg, sb)
        if seg in cur:
            for pb in (v.cp_addr,):
                wr(pb + ssum + cur.index(seg), sb)
    blocks = {}
    for seg, t in types.items():
        blocks.setdefault(seg // fsck.SIT_PER_BLK, bytearray(v.blk(v.sit_blk + seg // fsck.SIT_PER_BLK)))
        b = blocks[seg // fsck.SIT_PER_BLK]
        o = (seg % fsck.SIT_PER_BLK) * fsck.SIT_ENTRY
        struct.pack_into("<H", b, o, (fsck.r16(b, o) & 0x3FF) | (t << 10))
    for n, b in blocks.items():
        wr(v.sit_blk + n, b)
    nb = bytearray(v.blk(v.nat_blk))
    for i in (1, 2):
        struct.pack_into("<BII", nb, i * fsck.NAT_ENTRY, 0, i, 1)
    wr(v.nat_blk, nb)
    nodes = sum(1 for e in f.exp.values() if e[2])
    free = sum(1 for s in range(nmain) if s not in cur and (fsck.r16(blocks[s // fsck.SIT_PER_BLK],
               (s % fsck.SIT_PER_BLK) * fsck.SIT_ENTRY) & 0x3FF) == 0)
    inodes = len(f.inodes)
    for at in (v.cp_addr, v.cp_addr + total - 1):
        c = bytearray(v.raw(at))
        struct.pack_into("<I", c, 32, free)
        struct.pack_into("<I", c, 144, nodes)
        struct.pack_into("<I", c, 148, inodes)
        f2fslx.seal_cp(c)
        wr(at, c)
    print(f"{'would rebuild' if check else 'rebuilt'}: {len(sums)} segments with owners, {nodes} node blocks, "
          f"free_segment_count {free}, valid_inode_count {inodes}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
