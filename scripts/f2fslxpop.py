"""lx-format populated root image writer (the only root image format).

Used by scripts/mkfs-f2fs-populated.py, which gathers the tree. Writes it in
Linux's on-disk layout (scripts/f2fslx.py)
and the way mkfs.f2fs/the kernel lay a volume out so `fsck.f2fs --dry-run`
accepts it (mirrors scripts/mkfs-f2fs-home.py):

* two allocators: node blocks (inodes, direct/indirect nodes) fill HOT_NODE
  segments from the start of the main area; data blocks (file contents,
  dentry blocks) fill HOT_DATA segments right after; a segment holds one class;
* a typed SIT (type | valid count, MSB-first valid bitmap), indexed by
  main-area segment;
* an SSA block per main segment: (nid, version, ofs_in_node) for every block,
  footer = node/data;
* two checkpoint packs with the six current-segment summaries in the pack tail
  and a CRC, plus the servers/f2fs own-state words.
"""
import os
import struct
import uuid

import f2fslx

BLOCK = 4096
BPS = 512
F2FS_MAGIC = 0xF2F52010
CP_UMOUNT_FLAG = 0x1
NAT_ENTRY_SIZE = 9
SIT_ENTRY_SIZE = 74
SIT_PER_BLK = BLOCK // SIT_ENTRY_SIZE
MAX_ACTIVE_LOGS = 8
DT_DIR, DT_REG, DT_LNK = 4, 8, 10
S_IFMT, S_IFLNK = 0o170000, 0o120000
N = f2fslx.NIDS_PER_BLOCK
APB = f2fslx.ADDRS_PER_BLOCK


def w16(b, o, v): struct.pack_into("<H", b, o, v & 0xFFFF)
def w32(b, o, v): struct.pack_into("<I", b, o, v & 0xFFFFFFFF)
def w64(b, o, v): struct.pack_into("<Q", b, o, v)


class Pop:
    """Collects the file tree (same add_files_to_dir calls as the legacy
    packer), then lays it out in finish()."""

    def __init__(self, first_file_nid):
        self.next_nid = first_file_nid
        self.files = {}        # nid -> dict(inode fields)
        self.order = []        # nids in packing order
        self.entries = []      # (parent_ino, name bytes, nid, DT_*)
        self.by_path = {}      # host path -> nid

    def add(self, parent_ino, items):
        for entry in items:
            name, path, mode = entry[0], entry[1], entry[2]
            uid, gid = entry[3] if len(entry) > 3 else (0, 0)
            dt = DT_LNK if (mode & S_IFMT) == S_IFLNK else DT_REG
            key = None if isinstance(path, (bytes, bytearray)) else path
            if key is not None and key in self.by_path:
                nid = self.by_path[key]
                self.files[nid]["links"] += 1
                self.entries.append((parent_ino, name.encode(), nid, dt))
                continue
            nid = self.next_nid
            self.next_nid += 1
            mtime = None
            if key is not None:
                try:
                    mtime = int(os.path.getmtime(path))
                except OSError:
                    pass
            self.files[nid] = dict(path=path, mode=mode, uid=uid, gid=gid, links=1,
                                   pino=parent_ino, name=name.encode(), mtime=mtime)
            self.order.append(nid)
            if key is not None:
                self.by_path[key] = nid
            self.entries.append((parent_ino, name.encode(), nid, dt))

    def finish(self, out_path, total_segs, dir_nodes, dir_owner, subdirs, build_time, label=None):
        # ---- geometry (mirrors mkfs-f2fs-home.py) ----
        sit_blocks = -(-total_segs // SIT_PER_BLK)
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
        assert main_segs <= (sit_segs // 2) * BPS * SIT_PER_BLK
        nat_per = BLOCK // NAT_ENTRY_SIZE
        assert self.next_nid + 64 < (nat_segs // 2) * BPS * nat_per, "NAT too small"

        # ---- directories: dentry blocks ----
        parent_of = {3: 3}
        for p, kids in subdirs.items():
            for _n, c in kids:
                parent_of[c] = p
        dent = {ino: [(b".", ino, DT_DIR), (b"..", parent_of[ino], DT_DIR)] for ino in dir_nodes}
        for p, kids in subdirs.items():
            for nm, c in kids:
                dent[p].append((nm.encode(), c, DT_DIR))
        for parent, nm, nid, dt in self.entries:
            dent[parent].append((nm, nid, dt))
        dir_blocks = {}
        for ino in dir_nodes:
            dir_blocks[ino] = f2fslx.place_dentries(dent[ino])

        # ---- layout run: dry (count nodes/data) then real ----
        # Inode records: (nid, mode, uid, gid, links, pino, name, mtime, kind, payload)
        inodes = []
        for ino in sorted(dir_nodes):
            mode, uid, gid = dir_owner.get(ino, (0o040755, 0, 0))
            base = os.path.basename(dir_nodes[ino])
            inodes.append(dict(nid=ino, mode=mode, uid=uid, gid=gid,
                               links=2 + len(subdirs.get(ino, [])),
                               pino=parent_of[ino], name=base.encode(), mtime=None,
                               dir=True))
        for nid in self.order:
            f = self.files[nid]
            inodes.append(dict(nid=nid, mode=f["mode"], uid=f["uid"], gid=f["gid"],
                               links=f["links"], pino=f["pino"], name=f["name"],
                               mtime=f["mtime"], dir=False, path=f["path"]))

        meta = {}            # block -> bytearray (node blocks, NAT, SIT, SSA, cp)
        ssa = {}             # main segment -> bytearray summary block

        def run(dry, node_base, data_base, fout):
            nid_ctr = self.next_nid
            node_n = 0
            data_n = 0
            nat = {}

            def node_addr():
                nonlocal node_n
                a = node_base + node_n
                node_n += 1
                return a

            def sum_set(addr, nid, ofs, node):
                if dry:
                    return
                rel = addr - main_blk
                seg, off = rel // BPS, rel % BPS
                b = ssa.setdefault(seg, bytearray(BLOCK))
                struct.pack_into("<IBH", b, off * 7, nid, 1, ofs)
                b[4091] = 1 if node else 0

            def put_node(addr, b, nid, ino):
                if dry:
                    return
                meta[addr] = b
                nat[nid] = (addr, ino)

            for ino_rec in inodes:
                nid = ino_rec["nid"]
                if ino_rec["dir"]:
                    blocks, depth = dir_blocks[nid]
                    slots = [0] * (max(blocks) + 1)
                    payload = {}
                    for bi, db in sorted(blocks.items()):
                        slots[bi] = 1
                        payload[bi] = db
                    size = len(slots) * BLOCK
                else:
                    p = ino_rec["path"]
                    data = bytes(p) if isinstance(p, (bytes, bytearray)) else (
                        b"" if dry else open(p, "rb").read())
                    size = len(p) if isinstance(p, (bytes, bytearray)) else os.path.getsize(p)
                    slots = [1] * ((size + BLOCK - 1) // BLOCK)
                    payload = None
                    depth = 0
                # data addresses
                addrs = []
                for i, s in enumerate(slots):
                    if s:
                        a = data_base + data_n
                        data_n += 1
                        addrs.append(a)
                        if not dry:
                            if payload is not None:
                                chunk = payload[i]
                            else:
                                chunk = data[i * BLOCK:(i + 1) * BLOCK]
                            fout.seek(a * BLOCK)
                            fout.write(chunk)
                    else:
                        addrs.append(0)
                # inode block
                inode_at = node_addr()
                ib = bytearray(BLOCK)
                nodes_extra = 0
                i_nids = [0] * 5

                def owner_sums(lst, owner_nid, base_i=0):
                    for k, a in enumerate(lst):
                        if a:
                            sum_set(a, owner_nid, k, False)

                for k, a in enumerate(addrs[:f2fslx.ADDRS_PER_INODE]):
                    w32(ib, f2fslx.I_ADDR + 4 * k, a)
                owner_sums(addrs[:f2fslx.ADDRS_PER_INODE], nid)
                rest = addrs[f2fslx.ADDRS_PER_INODE:]
                chunks = [rest[i:i + APB] for i in range(0, len(rest), APB)]
                ind_state = {}
                for c, ch in enumerate(chunks):
                    if not any(ch):
                        continue
                    if c < 2:
                        ofs = f2fslx.ofs_dnode(c)
                        parent_ind = None
                    else:
                        j = c - 2
                        k, i = divmod(j, N)
                        if k >= 2:
                            raise ValueError("file too large for double-indirect-free layout")
                        ofs = f2fslx.ofs_ind_child(k, i)
                        parent_ind = k
                    dn_nid = nid_ctr; nid_ctr += 1
                    dn_at = node_addr(); nodes_extra += 1
                    db = bytearray(BLOCK)
                    for k2, a in enumerate(ch):
                        w32(db, 4 * k2, a)
                    f2fslx.write_footer(db, dn_nid, nid, ofs)
                    put_node(dn_at, db, dn_nid, nid)
                    sum_set(dn_at, dn_nid, 0, True)
                    owner_sums(ch, dn_nid)
                    if parent_ind is None:
                        i_nids[c] = dn_nid
                    else:
                        if parent_ind not in ind_state:
                            ind_nid = nid_ctr; nid_ctr += 1
                            ind_at = node_addr(); nodes_extra += 1
                            ind_state[parent_ind] = (ind_nid, ind_at, bytearray(BLOCK))
                            i_nids[2 + parent_ind] = ind_nid
                        ind_state[parent_ind][2][4 * i:4 * i + 4] = struct.pack("<I", dn_nid)
                for k, (ind_nid, ind_at, ibk) in ind_state.items():
                    f2fslx.write_footer(ibk, ind_nid, nid, f2fslx.ofs_ind(k))
                    put_node(ind_at, ibk, ind_nid, nid)
                    sum_set(ind_at, ind_nid, 0, True)
                for k, v in enumerate(i_nids):
                    w32(ib, f2fslx.I_NID + 4 * k, v)
                nonhole = sum(1 for a in addrs if a)
                w16(ib, f2fslx.I_MODE, ino_rec["mode"])
                w32(ib, f2fslx.I_UID, ino_rec["uid"])
                w32(ib, f2fslx.I_GID, ino_rec["gid"])
                w32(ib, f2fslx.I_LINKS, ino_rec["links"])
                w64(ib, f2fslx.I_SIZE, size)
                w64(ib, f2fslx.I_BLOCKS, 1 + nodes_extra + nonhole)
                m = int(build_time if ino_rec["mtime"] is None else ino_rec["mtime"])
                w64(ib, f2fslx.I_ATIME, m)
                w64(ib, f2fslx.I_MTIME, m)
                w64(ib, f2fslx.I_CTIME, max(m, build_time))
                w32(ib, f2fslx.I_CURRENT_DEPTH, depth if ino_rec["dir"] else 1)
                w32(ib, f2fslx.I_PINO, ino_rec["pino"])
                nm = ino_rec["name"][:f2fslx.NAME_LEN]
                w32(ib, f2fslx.I_NAMELEN, len(nm))
                ib[f2fslx.I_NAME:f2fslx.I_NAME + len(nm)] = nm
                f2fslx.write_footer(ib, nid, nid, 0)
                put_node(inode_at, ib, nid, nid)
                sum_set(inode_at, nid, 0, True)
            return node_n, data_n, nat

        node_total, data_total, _ = run(True, 0, 0, None)
        nseg = node_total // BPS + 1            # last one is the HOT_NODE log
        dseg = data_total // BPS + 1            # last one is the HOT_DATA log
        node_base = main_blk
        data_base = main_blk + nseg * BPS
        log_seg = {3: nseg - 1, 0: nseg + dseg - 1,
                   1: nseg + dseg, 2: nseg + dseg + 1,
                   4: nseg + dseg + 2, 5: nseg + dseg + 3}
        assert nseg + dseg + 4 < main_segs - 8, "image too small for the lx layout"

        tmp = out_path + ".tmp"
        with open(tmp, "wb") as fout:
            fout.truncate(total_blocks * BLOCK)
            _, _, nat = run(False, node_base, data_base, fout)
            n_inodes = len(inodes)

            def blk(n):
                if n not in meta:
                    meta[n] = bytearray(BLOCK)
                return meta[n]

            # Superblock (both copies).
            vol_uuid = uuid.uuid4().bytes
            for base in (0, 1):
                b = blk(base)
                s = 1024
                w32(b, s + 0, F2FS_MAGIC)
                w16(b, s + 4, 1); w16(b, s + 6, 12)
                w32(b, s + 8, 9); w32(b, s + 12, 3); w32(b, s + 16, 12)
                w32(b, s + 20, 9); w32(b, s + 24, 1); w32(b, s + 28, 1)
                w64(b, s + 36, total_blocks)
                w32(b, s + 44, main_segs)
                w32(b, s + 48, total_segs - 1)
                w32(b, s + 52, ckpt_segs); w32(b, s + 56, sit_segs)
                w32(b, s + 60, nat_segs); w32(b, s + 64, ssa_segs)
                w32(b, s + 68, main_segs); w32(b, s + 72, seg0)
                w32(b, s + 76, cp_blk); w32(b, s + 80, sit_blk)
                w32(b, s + 84, nat_blk); w32(b, s + 88, ssa_blk)
                w32(b, s + 92, main_blk)
                w32(b, s + 96, 3); w32(b, s + 100, 1); w32(b, s + 104, 2)
                b[s + 108:s + 124] = vol_uuid
                if label:   # volume_name, __le16[512] (root=LABEL=)
                    name = label.encode("utf-16-le")
                    b[s + 124:s + 124 + len(name)] = name
                f2fslx.mark_superblock(b, s)

            # NAT.
            for nid, (addr, owner) in nat.items():
                nb = blk(nat_blk + nid // nat_per)
                o = (nid % nat_per) * NAT_ENTRY_SIZE
                nb[o] = 1
                w32(nb, o + 1, owner)
                w32(nb, o + 5, addr)
            for dino in (1, 2):
                nb = blk(nat_blk + dino // nat_per)
                o = (dino % nat_per) * NAT_ENTRY_SIZE
                nb[o] = 0
                w32(nb, o + 1, dino)
                w32(nb, o + 5, 1)

            # SIT: per-segment valid counts and MSB-first bitmaps.
            node_per_seg = {}
            for i in range(node_total):
                node_per_seg[i // BPS] = node_per_seg.get(i // BPS, 0) + 1
            seg_info = {}          # segno -> (type, nvalid)
            for s_, c in node_per_seg.items():
                seg_info[s_] = (3, c)
            for i in range(data_total):
                s_ = nseg + i // BPS
                seg_info[s_] = (0, seg_info.get(s_, (0, 0))[1] + 1)
            for t in (3, 0, 1, 2, 4, 5):
                seg_info.setdefault(log_seg[t], ({0: 0, 1: 1, 2: 2, 3: 3, 4: 4, 5: 5}[t], 0))
            for s_, (typ, cnt) in seg_info.items():
                sb_ = blk(sit_blk + s_ // SIT_PER_BLK)
                o = (s_ % SIT_PER_BLK) * SIT_ENTRY_SIZE
                sb_[o:o + SIT_ENTRY_SIZE] = f2fslx.sit_entry(cnt, typ, range(cnt))
            # Empty typed logs still need their SSA footer.
            for t in range(6):
                b = ssa.setdefault(log_seg[t], bytearray(BLOCK))
                b[4091] = 1 if t >= 3 else 0
            for s_, b in ssa.items():
                meta[ssa_blk + s_] = b

            # Checkpoint packs.
            rsvd, ovp = f2fslx.checkpoint_reserve(ckpt_segs + sit_segs + nat_segs + ssa_segs)
            pack = (ckpt_segs // 2) * BPS
            used = node_total + data_total
            free_segs = main_segs - (nseg + dseg + 4)
            node_off = node_total % BPS
            data_off = data_total % BPS
            log_off = {3: node_off, 0: data_off}

            def checkpoint(at, ver):
                c = blk(at)
                w64(c, 0, ver)
                w64(c, 8, (main_segs - ovp) * BPS)
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
                w32(c, 140, pack - 7)
                w32(c, 144, node_total)          # valid_node_count
                w32(c, 148, n_inodes)            # valid_inode_count
                w32(c, 152, nid_next)            # next_free_nid
                w32(c, 156, (sit_segs // 2) * BPS // 8)
                w32(c, 160, (nat_segs // 2) * BPS // 8)
                for t in range(16):
                    c[176 + t] = 1
                w64(c, 2048, ver)
                w32(c, 3348, log_seg[0])
                w32(c, 3352, log_seg[3])
                w16(c, 3356, data_off)
                w16(c, 3358, node_off)
                f2fslx.seal_cp(c)
                for t in range(6):
                    src = ssa.get(log_seg[t], bytearray(BLOCK))
                    tb = bytearray(src)
                    tb[4091] = 1 if t >= 3 else 0
                    meta[at + pack - 7 + t] = tb
                meta[at + pack - 1] = bytearray(c)

            nid_next = max(max(nat), max(dir_nodes)) + 1
            checkpoint(cp_blk, 1)
            checkpoint(cp_blk + pack, 0)

            for n in sorted(meta):
                fout.seek(n * BLOCK)
                fout.write(meta[n])
        os.replace(tmp, out_path)
        return dict(total_blocks=total_blocks, main_segs=main_segs, node_blocks=node_total,
                    data_blocks=data_total, nseg=nseg, dseg=dseg, inodes=n_inodes)
