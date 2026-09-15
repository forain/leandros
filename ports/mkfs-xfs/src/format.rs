//! Lay an XFS v5 filesystem down on a device.
//!
//! The write plan mirrors libxfs' `xfs_ag_init_headers()` (the growfs path)
//! rather than mkfs' transactional path: each AG gets a superblock copy, an
//! AGF, an AGI, an empty AGFL and freshly initialised btree roots. AG 0
//! additionally gets the 64-inode chunk holding the root directory and the two
//! realtime metadata inodes; the log AG gets the internal log.
//!
//! Only blocks that must carry content are written, so formatting a 28 GiB
//! sparse file materialises roughly 150 KiB.

use std::io;

use crate::crc32c::stamp;
use crate::dev::Dev;
use crate::geom::Geom;
use crate::ondisk::*;

pub struct Params {
    pub geom: Geom,
    pub uuid: [u8; 16],
    pub label: String,
    /// Value for the root-namespace `xfs:autofsck` xattr on the root inode.
    pub autofsck: Option<String>,
    pub now_sec: i64,
    pub now_nsec: u32,
    pub gen: u32,
}

/// One allocated (non-free) region inside an AG, used to derive both the free
/// space btrees and the reverse mapping btree.
struct Used {
    start: u32,
    len: u32,
    owner: u64,
}

pub struct Layout {
    pub icount: u64,
    pub ifree: u64,
    pub fdblocks: u64,
    pub rootino: u64,
    pub rbmino: u64,
    pub rsumino: u64,
    /// AG 0 block at which the root inode chunk lives.
    pub ino_chunk_agbno: u32,
}

/// xfsprogs' mkfs hands AG 0 blocks 7..12 to the AGFL inside its allocation
/// transaction, so its first inode chunk lands on the next 64-inode-aligned
/// block (16) and sb_rootino comes out as 128. We leave the AGFL empty but
/// place the chunk at the same block anyway, so sb_rootino matches xfsprogs
/// byte for byte; the blocks below it simply stay free.
const XFSPROGS_AGFL_PREFILL: u32 = 6;

fn align_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

impl Params {
    pub fn layout(&self) -> Layout {
        let g = &self.geom;
        let prealloc = g.feat.prealloc_blocks();
        let ino_chunk_agbno = align_up(prealloc + XFSPROGS_AGFL_PREFILL, g.inoalignmt);
        let chunk_blocks = (XFS_INODES_PER_CHUNK * g.inodesize) / g.blocksize;

        let mut free = 0u64;
        for ag in 0..g.agcount {
            free += u64::from(g.ag_size(ag)) - u64::from(prealloc);
        }
        free -= u64::from(g.logblocks);
        free -= u64::from(chunk_blocks);

        let rootino = u64::from(ino_chunk_agbno) * u64::from(g.inopblock);
        Layout {
            icount: u64::from(XFS_INODES_PER_CHUNK),
            ifree: u64::from(XFS_INODES_PER_CHUNK) - 3,
            fdblocks: free,
            rootino,
            rbmino: rootino + 1,
            rsumino: rootino + 2,
            ino_chunk_agbno,
        }
    }

    /// Regions of AG `agno` that are *not* free, in ascending start order,
    /// excluding the static AG headers (blocks 0..prealloc).
    fn used_regions(&self, agno: u32, lay: &Layout) -> Vec<Used> {
        let g = &self.geom;
        let mut v = Vec::new();
        if agno == g.logagno {
            v.push(Used {
                start: g.logstart_agbno,
                len: g.logblocks,
                owner: XFS_RMAP_OWN_LOG,
            });
        }
        if agno == 0 {
            v.push(Used {
                start: lay.ino_chunk_agbno,
                len: (XFS_INODES_PER_CHUNK * g.inodesize) / g.blocksize,
                owner: XFS_RMAP_OWN_INODES,
            });
        }
        v.sort_by_key(|u| u.start);
        v
    }

    /// Free extents of AG `agno` in ascending start order.
    fn free_extents(&self, agno: u32, lay: &Layout) -> Vec<(u32, u32)> {
        let g = &self.geom;
        let agsize = g.ag_size(agno);
        let mut cur = g.feat.prealloc_blocks();
        let mut out = Vec::new();
        for u in self.used_regions(agno, lay) {
            if u.start > cur {
                out.push((cur, u.start - cur));
            }
            cur = u.start + u.len;
        }
        if agsize > cur {
            out.push((cur, agsize - cur));
        }
        out
    }

    // ------------------------------------------------------------- headers --

    pub fn superblock(&self, lay: &Layout) -> Vec<u8> {
        let g = &self.geom;
        let mut b = vec![0u8; g.sectorsize as usize];

        put32(&mut b, SB_MAGICNUM, XFS_SB_MAGIC);
        put32(&mut b, SB_BLOCKSIZE, g.blocksize);
        put64(&mut b, SB_DBLOCKS, g.dblocks);
        put64(&mut b, SB_RBLOCKS, 0);
        put64(&mut b, SB_REXTENTS, 0);
        b[SB_UUID..SB_UUID + 16].copy_from_slice(&self.uuid);
        put64(&mut b, SB_LOGSTART, g.logstart_fsb);
        put64(&mut b, SB_ROOTINO, lay.rootino);
        put64(&mut b, SB_RBMINO, lay.rbmino);
        put64(&mut b, SB_RSUMINO, lay.rsumino);
        put32(&mut b, SB_REXTSIZE, 1);
        put32(&mut b, SB_AGBLOCKS, g.agblocks);
        put32(&mut b, SB_AGCOUNT, g.agcount);
        put32(&mut b, SB_RBMBLOCKS, 0);
        put32(&mut b, SB_LOGBLOCKS, g.logblocks);

        let mut vers = XFS_SB_VERSION_5
            | XFS_SB_VERSION_NLINKBIT
            | XFS_SB_VERSION_ALIGNBIT
            | XFS_SB_VERSION_LOGV2BIT
            | XFS_SB_VERSION_EXTFLGBIT
            | XFS_SB_VERSION_DIRV2BIT
            | XFS_SB_VERSION_MOREBITSBIT;
        // v5 always carries attr2; mkfs additionally forces ATTRBIT on when
        // parent pointers are enabled so the kernel need not do it at mount.
        vers |= XFS_SB_VERSION_ATTRBIT;
        put16(&mut b, SB_VERSIONNUM, vers);

        put16(&mut b, SB_SECTSIZE, g.sectorsize as u16);
        put16(&mut b, SB_INODESIZE, g.inodesize as u16);
        put16(&mut b, SB_INOPBLOCK, g.inopblock as u16);

        let label = self.label.as_bytes();
        let n = label.len().min(12);
        b[SB_FNAME..SB_FNAME + n].copy_from_slice(&label[..n]);

        b[SB_BLOCKLOG] = g.blocklog;
        b[SB_SECTLOG] = g.sectlog;
        b[SB_INODELOG] = g.inodelog;
        b[SB_INOPBLOG] = g.inopblog;
        b[SB_AGBLKLOG] = g.agblklog;
        b[SB_REXTSLOG] = 0;
        b[SB_INPROGRESS] = 0;
        b[SB_IMAX_PCT] = g.imaxpct;

        put64(&mut b, SB_ICOUNT, lay.icount);
        put64(&mut b, SB_IFREE, lay.ifree);
        put64(&mut b, SB_FDBLOCKS, lay.fdblocks);
        put64(&mut b, SB_FREXTENTS, 0);
        put64(&mut b, SB_UQUOTINO, 0);
        put64(&mut b, SB_GQUOTINO, 0);
        put16(&mut b, SB_QFLAGS, 0);
        b[SB_FLAGS] = 0;
        b[SB_SHARED_VN] = 0;
        put32(&mut b, SB_INOALIGNMT, g.inoalignmt);
        put32(&mut b, SB_UNIT, 0);
        put32(&mut b, SB_WIDTH, 0);
        b[SB_DIRBLKLOG] = 0;
        b[SB_LOGSECTLOG] = 0;
        put16(&mut b, SB_LOGSECTSIZE, 0);
        put32(&mut b, SB_LOGSUNIT, 1);

        let f2 = XFS_SB_VERSION2_LAZYSBCOUNTBIT
            | XFS_SB_VERSION2_ATTR2BIT
            | XFS_SB_VERSION2_PROJID32BIT
            | XFS_SB_VERSION2_CRCBIT;
        // XFS_SB_VERSION2_FTYPE is a v4-only signal; on v5 ftype lives in
        // sb_features_incompat and mkfs leaves this bit clear.
        put32(&mut b, SB_FEATURES2, f2);
        put32(&mut b, SB_BAD_FEATURES2, f2);

        put32(&mut b, SB_FEATURES_COMPAT, 0);
        put32(&mut b, SB_FEATURES_RO_COMPAT, g.feat.ro_compat());
        put32(&mut b, SB_FEATURES_INCOMPAT, g.feat.incompat());
        put32(&mut b, SB_FEATURES_LOG_INCOMPAT, 0);
        put32(&mut b, SB_SPINO_ALIGN, g.spino_align);
        put64(&mut b, SB_PQUOTINO, 0);
        put64(&mut b, SB_LSN, 0);
        // META_UUID is not set, so sb_meta_uuid stays zero and every metadata
        // block carries sb_uuid instead.

        stamp(&mut b, SB_CRC);
        b
    }

    fn agf(&self, agno: u32, lay: &Layout) -> Vec<u8> {
        let g = &self.geom;
        let mut b = vec![0u8; g.sectorsize as usize];
        let free = self.free_extents(agno, lay);
        let freeblks: u32 = free.iter().map(|e| e.1).sum();
        let longest = free.iter().map(|e| e.1).max().unwrap_or(0);

        put32(&mut b, AGF_MAGICNUM, XFS_AGF_MAGIC);
        put32(&mut b, AGF_VERSIONNUM, 1);
        put32(&mut b, AGF_SEQNO, agno);
        put32(&mut b, AGF_LENGTH, g.ag_size(agno));
        put32(&mut b, AGF_BNO_ROOT, crate::geom::Features::BNO_BLOCK);
        put32(&mut b, AGF_CNT_ROOT, crate::geom::Features::CNT_BLOCK);
        put32(&mut b, AGF_BNO_LEVEL, 1);
        put32(&mut b, AGF_CNT_LEVEL, 1);
        if g.feat.rmapbt {
            put32(&mut b, AGF_RMAP_ROOT, g.feat.rmap_block());
            put32(&mut b, AGF_RMAP_LEVEL, 1);
            put32(&mut b, AGF_RMAP_BLOCKS, 1);
        }
        // Empty freelist, exactly as xfs_agflblock_init() leaves it.
        put32(&mut b, AGF_FLFIRST, 1);
        put32(&mut b, AGF_FLLAST, 0);
        put32(&mut b, AGF_FLCOUNT, 0);
        put32(&mut b, AGF_FREEBLKS, freeblks);
        put32(&mut b, AGF_LONGEST, longest);
        put32(&mut b, AGF_BTREEBLKS, 0);
        b[AGF_UUID..AGF_UUID + 16].copy_from_slice(&self.uuid);
        if g.feat.reflink {
            put32(&mut b, AGF_REFCOUNT_ROOT, g.feat.refc_block());
            put32(&mut b, AGF_REFCOUNT_LEVEL, 1);
            put32(&mut b, AGF_REFCOUNT_BLOCKS, 1);
        }
        put64(&mut b, AGF_LSN, 0);
        stamp(&mut b, AGF_CRC);
        b
    }

    fn agi(&self, agno: u32, lay: &Layout) -> Vec<u8> {
        let g = &self.geom;
        let mut b = vec![0u8; g.sectorsize as usize];
        let has_inodes = agno == 0;

        put32(&mut b, AGI_MAGICNUM, XFS_AGI_MAGIC);
        put32(&mut b, AGI_VERSIONNUM, 1);
        put32(&mut b, AGI_SEQNO, agno);
        put32(&mut b, AGI_LENGTH, g.ag_size(agno));
        put32(&mut b, AGI_COUNT, if has_inodes { XFS_INODES_PER_CHUNK } else { 0 });
        put32(&mut b, AGI_ROOT, crate::geom::Features::IBT_BLOCK);
        put32(&mut b, AGI_LEVEL, 1);
        put32(
            &mut b,
            AGI_FREECOUNT,
            if has_inodes { XFS_INODES_PER_CHUNK - 3 } else { 0 },
        );
        put32(
            &mut b,
            AGI_NEWINO,
            if has_inodes { lay.rootino as u32 } else { NULLAGINO },
        );
        put32(&mut b, AGI_DIRINO, NULLAGINO);
        for i in 0..64 {
            put32(&mut b, AGI_UNLINKED + i * 4, NULLAGINO);
        }
        b[AGI_UUID..AGI_UUID + 16].copy_from_slice(&self.uuid);
        put64(&mut b, AGI_LSN, 0);
        if g.feat.finobt {
            put32(&mut b, AGI_FREE_ROOT, crate::geom::Features::FIBT_BLOCK);
            put32(&mut b, AGI_FREE_LEVEL, 1);
        }
        if g.feat.inobtcount {
            put32(&mut b, AGI_IBLOCKS, 1);
            if g.feat.finobt {
                put32(&mut b, AGI_FBLOCKS, 1);
            }
        }
        stamp(&mut b, AGI_CRC);
        b
    }

    fn agfl(&self, agno: u32) -> Vec<u8> {
        let g = &self.geom;
        let mut b = vec![0u8; g.sectorsize as usize];
        put32(&mut b, AGFL_MAGICNUM, XFS_AGFL_MAGIC);
        put32(&mut b, AGFL_SEQNO, agno);
        b[AGFL_UUID..AGFL_UUID + 16].copy_from_slice(&self.uuid);
        put64(&mut b, AGFL_LSN, 0);
        let n = (g.sectorsize as usize - AGFL_HDR_LEN) / 4;
        for i in 0..n {
            put32(&mut b, AGFL_HDR_LEN + i * 4, NULLAGBLOCK);
        }
        stamp(&mut b, AGFL_CRC);
        b
    }

    // -------------------------------------------------------- btree blocks --

    fn btree_block(&self, magic: u32, agno: u32, agbno: u32, numrecs: u16) -> Vec<u8> {
        let g = &self.geom;
        let mut b = vec![0u8; g.blocksize as usize];
        put32(&mut b, BB_MAGIC, magic);
        put16(&mut b, BB_LEVEL, 0);
        put16(&mut b, BB_NUMRECS, numrecs);
        put32(&mut b, BB_LEFTSIB, NULLAGBLOCK);
        put32(&mut b, BB_RIGHTSIB, NULLAGBLOCK);
        put64(&mut b, BB_BLKNO, g.ag_daddr(agno, agbno));
        put64(&mut b, BB_LSN, 0);
        b[BB_UUID..BB_UUID + 16].copy_from_slice(&self.uuid);
        put32(&mut b, BB_OWNER, agno);
        b
    }

    fn alloc_root(&self, magic: u32, agno: u32, agbno: u32, recs: &[(u32, u32)]) -> Vec<u8> {
        let mut b = self.btree_block(magic, agno, agbno, recs.len() as u16);
        for (i, (start, len)) in recs.iter().enumerate() {
            let o = SBLOCK_CRC_LEN + i * 8;
            put32(&mut b, o, *start);
            put32(&mut b, o + 4, *len);
        }
        stamp(&mut b, BB_CRC);
        b
    }

    fn rmap_root(&self, agno: u32, lay: &Layout) -> Vec<u8> {
        let g = &self.geom;
        let f = &g.feat;
        // Same records, in the same order, as libxfs' xfs_rmaproot_init().
        let mut recs: Vec<(u32, u32, u64)> = vec![
            (0, crate::geom::Features::BNO_BLOCK, XFS_RMAP_OWN_FS),
            (crate::geom::Features::BNO_BLOCK, 2, XFS_RMAP_OWN_AG),
            (
                crate::geom::Features::IBT_BLOCK,
                f.rmap_block() - crate::geom::Features::IBT_BLOCK,
                XFS_RMAP_OWN_INOBT,
            ),
            (f.rmap_block(), 1, XFS_RMAP_OWN_AG),
        ];
        if f.reflink {
            recs.push((f.refc_block(), 1, XFS_RMAP_OWN_REFC));
        }
        for u in self.used_regions(agno, lay) {
            recs.push((u.start, u.len, u.owner));
        }

        let mut b = self.btree_block(XFS_RMAP_CRC_MAGIC, agno, f.rmap_block(), recs.len() as u16);
        for (i, (start, len, owner)) in recs.iter().enumerate() {
            let o = SBLOCK_CRC_LEN + i * 24;
            put32(&mut b, o, *start);
            put32(&mut b, o + 4, *len);
            put64(&mut b, o + 8, *owner);
            put64(&mut b, o + 16, 0);
        }
        stamp(&mut b, BB_CRC);
        b
    }

    fn inobt_root(&self, magic: u32, agno: u32, agbno: u32, lay: &Layout) -> Vec<u8> {
        let has = agno == 0;
        let mut b = self.btree_block(magic, agno, agbno, if has { 1 } else { 0 });
        if has {
            let o = SBLOCK_CRC_LEN;
            put32(&mut b, o, lay.rootino as u32); // ir_startino (agino)
            put16(&mut b, o + 4, 0); // ir_holemask
            b[o + 6] = XFS_INODES_PER_CHUNK as u8; // ir_count
            b[o + 7] = (XFS_INODES_PER_CHUNK - 3) as u8; // ir_freecount
            // ir_free: bit N set == inode N free. The first three are the root
            // directory and the two realtime metadata inodes.
            put64(&mut b, o + 8, !0u64 << 3);
        }
        stamp(&mut b, BB_CRC);
        b
    }

    // -------------------------------------------------------------- inodes --

    fn timestamp(&self, sec: i64, nsec: u32) -> [u8; 8] {
        if self.geom.feat.bigtime {
            let v = ((sec + XFS_BIGTIME_EPOCH_OFFSET) as u64) * 1_000_000_000 + u64::from(nsec);
            v.to_be_bytes()
        } else {
            let mut o = [0u8; 8];
            o[0..4].copy_from_slice(&(sec as i32).to_be_bytes());
            o[4..8].copy_from_slice(&nsec.to_be_bytes());
            o
        }
    }

    fn inode_core(&self, ino: u64, mode: u16, nlink: u32, format: u8, gen: u32) -> Vec<u8> {
        let g = &self.geom;
        let mut b = vec![0u8; g.inodesize as usize];
        put16(&mut b, DI_MAGIC, XFS_DINODE_MAGIC);
        put16(&mut b, DI_MODE, mode);
        b[DI_VERSION] = 3;
        b[DI_FORMAT] = format;
        put32(&mut b, DI_NLINK, nlink);
        let ts = self.timestamp(self.now_sec, self.now_nsec);
        b[DI_ATIME..DI_ATIME + 8].copy_from_slice(&ts);
        b[DI_MTIME..DI_MTIME + 8].copy_from_slice(&ts);
        b[DI_CTIME..DI_CTIME + 8].copy_from_slice(&ts);
        b[DI_CRTIME..DI_CRTIME + 8].copy_from_slice(&ts);
        b[DI_AFORMAT] = XFS_DINODE_FMT_EXTENTS;
        put32(&mut b, DI_GEN, gen);
        put32(&mut b, DI_NEXT_UNLINKED, NULLAGINO);
        put64(&mut b, DI_CHANGECOUNT, 1);
        let mut f2 = 0u64;
        if g.feat.bigtime {
            f2 |= XFS_DIFLAG2_BIGTIME;
        }
        if g.feat.nrext64 {
            f2 |= XFS_DIFLAG2_NREXT64;
        }
        put64(&mut b, DI_FLAGS2, f2);
        put64(&mut b, DI_INO, ino);
        b[DI_UUID..DI_UUID + 16].copy_from_slice(&self.uuid);
        b
    }

    /// The root directory: an empty shortform directory, optionally carrying
    /// the `xfs:autofsck` root-namespace xattr in a shortform attribute fork.
    fn root_inode(&self, lay: &Layout) -> Vec<u8> {
        let mut b = self.inode_core(
            lay.rootino,
            S_IFDIR | 0o755,
            2,
            XFS_DINODE_FMT_LOCAL,
            0,
        );

        // xfs_dir2_sf_hdr: count, i8count, then the parent inode number, which
        // is 4 bytes wide while i8count is 0. An empty root points at itself.
        let d = DI_LITERAL;
        b[d] = 0; // count
        b[d + 1] = 0; // i8count
        put32(&mut b, d + 2, lay.rootino as u32);
        put64(&mut b, DI_SIZE, 6);

        if let Some(value) = &self.autofsck {
            let name = b"xfs:autofsck";
            let val = value.as_bytes();
            // xfs_attr_sf_hdr (4) + xfs_attr_sf_entry (3) + name + value.
            let attr_len = 4 + 3 + name.len() + val.len();
            // libxfs' xfs_attr_shortform_bytesfit(): put the fork boundary as
            // high as the attribute needs, but never above maxforkoff, which
            // reserves room for a minimal attr-fork btree root.
            let litino = self.geom.inodesize as usize - DI_LITERAL;
            const BMDR_SPACE_MINABTPTRS: usize = 4 + 2 * 16;
            let maxforkoff = (litino - BMDR_SPACE_MINABTPTRS) / 8;
            let forkoff = ((litino - attr_len) / 8).min(maxforkoff) as u8;
            let aoff = DI_LITERAL + usize::from(forkoff) * 8;
            assert!(usize::from(forkoff) * 8 >= 6, "data fork does not fit");
            assert!(aoff + attr_len <= self.geom.inodesize as usize);
            b[DI_FORKOFF] = forkoff;
            b[DI_AFORMAT] = XFS_DINODE_FMT_LOCAL;
            put16(&mut b, aoff, attr_len as u16); // totsize
            b[aoff + 2] = 1; // count
            b[aoff + 3] = 0; // padding
            b[aoff + 4] = name.len() as u8;
            b[aoff + 5] = val.len() as u8;
            b[aoff + 6] = XFS_ATTR_ROOT;
            b[aoff + 7..aoff + 7 + name.len()].copy_from_slice(name);
            b[aoff + 7 + name.len()..aoff + 7 + name.len() + val.len()].copy_from_slice(val);
        }

        stamp(&mut b, DI_CRC);
        b
    }

    fn rt_inode(&self, ino: u64, newrtbm: bool) -> Vec<u8> {
        let mut b = self.inode_core(ino, S_IFREG, 1, XFS_DINODE_FMT_EXTENTS, self.gen);
        if newrtbm {
            put16(&mut b, DI_FLAGS, XFS_DIFLAG_NEWRTBM);
        }
        stamp(&mut b, DI_CRC);
        b
    }

    /// A never-allocated inode inside an allocated chunk. libxfs'
    /// xfs_ialloc_inode_init() stamps magic, version, generation, inode number
    /// and UUID into every one of them and checksums it.
    fn free_inode(&self, ino: u64) -> Vec<u8> {
        let mut b = vec![0u8; self.geom.inodesize as usize];
        put16(&mut b, DI_MAGIC, XFS_DINODE_MAGIC);
        b[DI_VERSION] = 3;
        put32(&mut b, DI_GEN, self.gen);
        put32(&mut b, DI_NEXT_UNLINKED, NULLAGINO);
        put64(&mut b, DI_INO, ino);
        b[DI_UUID..DI_UUID + 16].copy_from_slice(&self.uuid);
        stamp(&mut b, DI_CRC);
        b
    }

    // ----------------------------------------------------------------- log --

    /// The two 512-byte blocks libxfs_log_header() writes at the head of a
    /// freshly formatted internal log: a v2 record header whose single
    /// operation is an unmount record. The rest of the log stays zeroed, which
    /// is how the kernel recognises a clean, never-used log.
    fn log_head(&self) -> Vec<u8> {
        let mut b = vec![0u8; 2 * BBSIZE];
        let cycle = XLOG_INIT_CYCLE;
        let lsn = u64::from(cycle) << 32;

        // Block 1: the unmount record, built first so its leading word can be
        // packed into the header.
        let u = BBSIZE;
        put32(&mut b, u, USERSPACE_TID);
        put32(&mut b, u + 4, 8); // oh_len
        b[u + 8] = XFS_LOG_CLIENT;
        b[u + 9] = XLOG_UNMOUNT_TRANS;
        put16(&mut b, u + 10, 0); // oh_res2
        // The payload is written in host (little) endian, as libxfs does.
        b[u + 12..u + 14].copy_from_slice(&XLOG_UNMOUNT_TYPE.to_le_bytes());

        // Block 0: the record header.
        put32(&mut b, LOG_H_MAGICNO, XLOG_HEADER_MAGIC_NUM);
        put32(&mut b, LOG_H_CYCLE, cycle);
        put32(&mut b, LOG_H_VERSION, 2);
        put32(&mut b, LOG_H_LEN, BBSIZE as u32);
        put64(&mut b, LOG_H_LSN, lsn);
        put64(&mut b, LOG_H_TAIL_LSN, lsn);
        // libxfs leaves h_crc zero for the initial record.
        put32(&mut b, LOG_H_CRC, 0);
        put32(&mut b, LOG_H_PREV_BLOCK, 0xffff_ffff);
        put32(&mut b, LOG_H_NUM_LOGOPS, 1);
        put32(&mut b, LOG_H_FMT, XLOG_FMT_LINUX_LE);
        b[LOG_H_FS_UUID..LOG_H_FS_UUID + 16].copy_from_slice(&self.uuid);
        put32(&mut b, LOG_H_SIZE, XLOG_BIG_RECORD_BSIZE);

        // Pack the first word of the data block into the header and replace it
        // with the cycle number, so every log block starts with the cycle.
        let first: [u8; 4] = b[u..u + 4].try_into().unwrap();
        b[LOG_H_CYCLE_DATA..LOG_H_CYCLE_DATA + 4].copy_from_slice(&first);
        put32(&mut b, u, cycle);

        b
    }

    // --------------------------------------------------------------- write --

    pub fn write(&self, dev: &Dev) -> io::Result<Layout> {
        let g = &self.geom;
        let lay = self.layout();
        let sb = self.superblock(&lay);
        let bs = g.blocksize as usize;
        let ss = g.sectorsize as usize;

        // Wipe any foreign signature at the head of the device, then the tail
        // block, the way mkfs does. Both are "zero only if not already zero"
        // so that a sparse image stays sparse.
        dev.zero_range(0, WHACK_SIZE.min(g.dblocks * u64::from(g.blocksize)))?;
        dev.zero_range((g.dblocks - 1) * u64::from(g.blocksize), u64::from(g.blocksize))?;

        for ag in 0..g.agcount {
            // AG block 0 holds the superblock copy, AGF, AGI and AGFL, one
            // sector each.
            let mut blk0 = vec![0u8; bs];
            blk0[0..ss].copy_from_slice(&sb);
            blk0[ss..2 * ss].copy_from_slice(&self.agf(ag, &lay));
            blk0[2 * ss..3 * ss].copy_from_slice(&self.agi(ag, &lay));
            blk0[3 * ss..4 * ss].copy_from_slice(&self.agfl(ag));
            if ag != 0 {
                dev.write_at(g.ag_byte(ag, 0), &blk0)?;
            } else {
                // Leave the primary superblock for last; write the rest of the
                // block now so a torn format cannot look mountable.
                dev.write_at(g.ag_byte(ag, 0) + ss as u64, &blk0[ss..])?;
            }

            let free = self.free_extents(ag, &lay);
            let by_bno: Vec<(u32, u32)> = free.clone();
            let mut by_cnt = free.clone();
            by_cnt.sort_by_key(|&(s, l)| (l, s));

            dev.write_at(
                g.ag_byte(ag, crate::geom::Features::BNO_BLOCK),
                &self.alloc_root(
                    XFS_ABTB_CRC_MAGIC,
                    ag,
                    crate::geom::Features::BNO_BLOCK,
                    &by_bno,
                ),
            )?;
            dev.write_at(
                g.ag_byte(ag, crate::geom::Features::CNT_BLOCK),
                &self.alloc_root(
                    XFS_ABTC_CRC_MAGIC,
                    ag,
                    crate::geom::Features::CNT_BLOCK,
                    &by_cnt,
                ),
            )?;
            dev.write_at(
                g.ag_byte(ag, crate::geom::Features::IBT_BLOCK),
                &self.inobt_root(
                    XFS_IBT_CRC_MAGIC,
                    ag,
                    crate::geom::Features::IBT_BLOCK,
                    &lay,
                ),
            )?;
            if g.feat.finobt {
                dev.write_at(
                    g.ag_byte(ag, crate::geom::Features::FIBT_BLOCK),
                    &self.inobt_root(
                        XFS_FIBT_CRC_MAGIC,
                        ag,
                        crate::geom::Features::FIBT_BLOCK,
                        &lay,
                    ),
                )?;
            }
            if g.feat.rmapbt {
                dev.write_at(g.ag_byte(ag, g.feat.rmap_block()), &self.rmap_root(ag, &lay))?;
            }
            if g.feat.reflink {
                let b = {
                    let mut b =
                        self.btree_block(XFS_REFC_CRC_MAGIC, ag, g.feat.refc_block(), 0);
                    stamp(&mut b, BB_CRC);
                    b
                };
                dev.write_at(g.ag_byte(ag, g.feat.refc_block()), &b)?;
            }
        }

        // The root inode chunk in AG 0.
        let chunk_blocks = (XFS_INODES_PER_CHUNK * g.inodesize) / g.blocksize;
        let mut chunk = Vec::with_capacity((chunk_blocks * g.blocksize) as usize);
        for i in 0..u64::from(XFS_INODES_PER_CHUNK) {
            let ino = lay.rootino + i;
            let inode = match i {
                0 => self.root_inode(&lay),
                1 => self.rt_inode(ino, true),
                2 => self.rt_inode(ino, false),
                _ => self.free_inode(ino),
            };
            chunk.extend_from_slice(&inode);
        }
        dev.write_at(g.ag_byte(0, lay.ino_chunk_agbno), &chunk)?;

        // The internal log: zero it (only where it is not already zero) and
        // then stamp the clean-log record at its head.
        let log_off = g.ag_byte(g.logagno, g.logstart_agbno);
        dev.zero_range(log_off, u64::from(g.logblocks) * u64::from(g.blocksize))?;
        dev.write_at(log_off, &self.log_head())?;

        // Finally the primary superblock, which is what makes the device a
        // filesystem.
        dev.write_at(0, &sb)?;
        dev.sync()?;
        Ok(lay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{self, Features, GeomRequest};

    fn params(bytes: u64) -> Params {
        let geom = geom::compute(&GeomRequest {
            devsize: bytes,
            feat: Features {
                exchange: true,
                parent: true,
                ..Features::default()
            },
            dsize_blocks: None,
            agcount: None,
            agsize: None,
            logsize_blocks: None,
            logagno: None,
        })
        .unwrap();
        Params {
            geom,
            uuid: [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef,
            ],
            label: "ROOT".into(),
            autofsck: Some("repair".into()),
            now_sec: 1_789_437_758,
            now_nsec: 498_959_000,
            gen: 0,
        }
    }

    #[test]
    fn layout_matches_reference_image() {
        let p = params(28 << 30);
        let l = p.layout();
        assert_eq!(l.ino_chunk_agbno, 16);
        assert_eq!(l.rootino, 128);
        assert_eq!(l.rbmino, 129);
        assert_eq!(l.rsumino, 130);
        assert_eq!(l.icount, 64);
        assert_eq!(l.ifree, 61);
        // Real mkfs.xfs 7.1.1 reports exactly this for a 28 GiB image.
        assert_eq!(l.fdblocks, 7_323_612);
    }

    #[test]
    fn superblock_fields_match_reference() {
        let p = params(28 << 30);
        let l = p.layout();
        let sb = p.superblock(&l);
        let be16 = |o: usize| u16::from_be_bytes(sb[o..o + 2].try_into().unwrap());
        let be32 = |o: usize| u32::from_be_bytes(sb[o..o + 4].try_into().unwrap());
        let be64 = |o: usize| u64::from_be_bytes(sb[o..o + 8].try_into().unwrap());
        assert_eq!(be32(SB_MAGICNUM), XFS_SB_MAGIC);
        assert_eq!(be64(SB_DBLOCKS), 7_340_032);
        assert_eq!(be64(SB_LOGSTART), 4_194_311);
        assert_eq!(be16(SB_VERSIONNUM), 0xb4b5);
        assert_eq!(be32(SB_FEATURES2), 0x18a);
        assert_eq!(be32(SB_BAD_FEATURES2), 0x18a);
        assert_eq!(be32(SB_FEATURES_RO_COMPAT), 0xf);
        assert_eq!(be32(SB_FEATURES_INCOMPAT), 0xeb);
        assert_eq!(be32(SB_SPINO_ALIGN), 4);
        assert_eq!(be32(SB_INOALIGNMT), 8);
        assert_eq!(be32(SB_LOGSUNIT), 1);
        assert_eq!(sb[SB_AGBLKLOG], 21);
        assert_eq!(sb[SB_IMAX_PCT], 25);
    }

    #[test]
    fn root_inode_matches_reference_bytes() {
        let p = params(28 << 30);
        let l = p.layout();
        let i = p.root_inode(&l);
        assert_eq!(i[DI_FORKOFF], 37);
        assert_eq!(i[DI_AFORMAT], XFS_DINODE_FMT_LOCAL);
        assert_eq!(u64::from_be_bytes(i[DI_SIZE..DI_SIZE + 8].try_into().unwrap()), 6);
        let aoff = DI_LITERAL + 37 * 8;
        assert_eq!(u16::from_be_bytes(i[aoff..aoff + 2].try_into().unwrap()), 25);
        assert_eq!(i[aoff + 6], XFS_ATTR_ROOT);
        assert_eq!(&i[aoff + 7..aoff + 19], b"xfs:autofsck");
        // Timestamps: bigtime, as emitted by real mkfs for the same instant.
        assert_eq!(
            u64::from_be_bytes(i[DI_ATIME..DI_ATIME + 8].try_into().unwrap()),
            0x36a2_c129_1700_6e98
        );
    }

    #[test]
    fn free_space_of_each_ag() {
        let p = params(28 << 30);
        let l = p.layout();
        assert_eq!(p.free_extents(0, &l), vec![(7, 9), (24, 1_834_984)]);
        assert_eq!(p.free_extents(1, &l), vec![(7, 1_835_001)]);
        assert_eq!(p.free_extents(2, &l), vec![(16_391, 1_818_617)]);
        assert_eq!(p.free_extents(3, &l), vec![(7, 1_835_001)]);
    }

    #[test]
    fn log_head_matches_reference_bytes() {
        let p = params(28 << 30);
        let b = p.log_head();
        assert_eq!(&b[0..4], &[0xfe, 0xed, 0xba, 0xbe]);
        assert_eq!(u32::from_be_bytes(b[LOG_H_LEN..LOG_H_LEN + 4].try_into().unwrap()), 512);
        assert_eq!(
            u64::from_be_bytes(b[LOG_H_LSN..LOG_H_LSN + 8].try_into().unwrap()),
            1u64 << 32
        );
        assert_eq!(
            u32::from_be_bytes(
                b[LOG_H_CYCLE_DATA..LOG_H_CYCLE_DATA + 4].try_into().unwrap()
            ),
            USERSPACE_TID
        );
        assert_eq!(u32::from_be_bytes(b[LOG_H_FMT..LOG_H_FMT + 4].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_be_bytes(b[LOG_H_SIZE..LOG_H_SIZE + 4].try_into().unwrap()),
            32768
        );
        assert_eq!(u32::from_be_bytes(b[512..516].try_into().unwrap()), 1);
        assert_eq!(u32::from_be_bytes(b[516..520].try_into().unwrap()), 8);
        assert_eq!(b[520], 0xaa);
        assert_eq!(b[521], 0x20);
        assert_eq!(&b[524..526], &[0x6e, 0x55]);
    }
}
