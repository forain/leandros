//! Filesystem geometry: what xfsprogs' mkfs would choose for a device of a
//! given size, reimplemented from libxfs/topology.c (calc_default_ag_geometry)
//! and mkfs/xfs_mkfs.c (calculate_log_size, calculate_imaxpct).

use crate::ondisk::*;

/// v5 feature set. Everything except `crc` is individually switchable from the
/// command line; `crc` is mandatory because this formatter only writes v5.
#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub ftype: bool,
    pub finobt: bool,
    pub sparse: bool,
    pub rmapbt: bool,
    pub reflink: bool,
    pub bigtime: bool,
    pub inobtcount: bool,
    pub nrext64: bool,
    pub exchange: bool,
    pub parent: bool,
}

impl Default for Features {
    /// mkfs.xfs 7.x defaults for a fresh v5 filesystem.
    fn default() -> Self {
        Features {
            ftype: true,
            finobt: true,
            sparse: true,
            rmapbt: true,
            reflink: true,
            bigtime: true,
            inobtcount: true,
            nrext64: true,
            exchange: false,
            parent: false,
        }
    }
}

impl Features {
    // The fixed block numbers inside every AG. With sectorsize < blocksize the
    // SB, AGF, AGI and AGFL all share AG block 0 (one sector each), so the
    // btree roots start at block 1. See XFS_BNO_BLOCK() and friends in
    // libxfs/xfs_format.h.
    pub const BNO_BLOCK: u32 = 1;
    pub const CNT_BLOCK: u32 = 2;
    pub const IBT_BLOCK: u32 = 3;
    pub const FIBT_BLOCK: u32 = 4;

    pub fn rmap_block(&self) -> u32 {
        if self.finobt {
            Self::FIBT_BLOCK + 1
        } else {
            Self::IBT_BLOCK + 1
        }
    }

    pub fn refc_block(&self) -> u32 {
        if self.rmapbt {
            self.rmap_block() + 1
        } else if self.finobt {
            Self::FIBT_BLOCK + 1
        } else {
            Self::IBT_BLOCK + 1
        }
    }

    /// libxfs_prealloc_blocks(): AG blocks consumed by the static headers.
    pub fn prealloc_blocks(&self) -> u32 {
        if self.reflink {
            self.refc_block() + 1
        } else if self.rmapbt {
            self.rmap_block() + 1
        } else if self.finobt {
            Self::FIBT_BLOCK + 1
        } else {
            Self::IBT_BLOCK + 1
        }
    }

    pub fn ro_compat(&self) -> u32 {
        let mut v = 0;
        if self.finobt {
            v |= XFS_SB_FEAT_RO_COMPAT_FINOBT;
        }
        if self.rmapbt {
            v |= XFS_SB_FEAT_RO_COMPAT_RMAPBT;
        }
        if self.reflink {
            v |= XFS_SB_FEAT_RO_COMPAT_REFLINK;
        }
        if self.inobtcount {
            v |= XFS_SB_FEAT_RO_COMPAT_INOBTCNT;
        }
        v
    }

    pub fn incompat(&self) -> u32 {
        let mut v = 0;
        if self.ftype {
            v |= XFS_SB_FEAT_INCOMPAT_FTYPE;
        }
        if self.sparse {
            v |= XFS_SB_FEAT_INCOMPAT_SPINODES;
        }
        if self.bigtime {
            v |= XFS_SB_FEAT_INCOMPAT_BIGTIME;
        }
        if self.nrext64 {
            v |= XFS_SB_FEAT_INCOMPAT_NREXT64;
        }
        if self.exchange {
            v |= XFS_SB_FEAT_INCOMPAT_EXCHRANGE;
        }
        if self.parent {
            v |= XFS_SB_FEAT_INCOMPAT_PARENT;
        }
        v
    }
}

#[derive(Clone, Debug)]
pub struct Geom {
    pub blocksize: u32,
    pub blocklog: u8,
    pub sectorsize: u32,
    pub sectlog: u8,
    pub inodesize: u32,
    pub inodelog: u8,
    pub inopblock: u32,
    pub inopblog: u8,

    pub dblocks: u64,
    pub agblocks: u32,
    pub agcount: u32,
    pub agblklog: u8,
    pub imaxpct: u8,
    pub inoalignmt: u32,
    pub spino_align: u32,

    pub logblocks: u32,
    pub logagno: u32,
    /// AG block at which the internal log starts (== prealloc_blocks).
    pub logstart_agbno: u32,
    /// sb_logstart, in the agno<<agblklog | agbno encoding.
    pub logstart_fsb: u64,

    pub feat: Features,
}

fn ilog2_ceil(mut v: u64) -> u8 {
    let mut n = 0u8;
    let mut p = 1u64;
    while p < v {
        p <<= 1;
        n += 1;
    }
    let _ = &mut v;
    n
}

/// libxfs/topology.c calc_default_ag_geometry(), single-disk path only: this
/// formatter never probes for RAID geometry, so `multidisk` is always 0.
fn calc_default_ag_geometry(blocklog: u8, dblocks: u64) -> (u64, u64) {
    let bl = blocklog as u32;
    let terabytes = |n: u64| n << (40 - bl);
    let megabytes = |n: u64| n << (20 - bl);
    let ag_max_blocks = (XFS_AG_MAX_BYTES - 1) >> bl;

    let blocks;
    if dblocks >= terabytes(32) || dblocks >= terabytes(4) {
        blocks = ag_max_blocks;
    } else if dblocks >= megabytes(128) {
        blocks = shift_up(dblocks, XFS_NOMULTIDISK_AGLOG, ag_max_blocks);
    } else {
        let mut shift = XFS_MULTIDISK_AGLOG;
        if dblocks <= (512u64 << (30 - bl)) {
            shift -= 1;
        }
        if dblocks <= (8u64 << (30 - bl)) {
            shift -= 1;
        }
        if dblocks < megabytes(128) {
            shift -= 1;
        }
        if dblocks < megabytes(64) {
            shift -= 1;
        }
        if dblocks < megabytes(32) {
            shift -= 1;
        }
        blocks = shift_up(dblocks, shift, ag_max_blocks);
    }
    let blocks = blocks.max(1);
    let agcount = dblocks.div_ceil(blocks);
    (blocks, agcount)
}

fn shift_up(dblocks: u64, shift: u32, ag_max_blocks: u64) -> u64 {
    let mut blocks = dblocks >> shift;
    if dblocks & ((1u64 << shift) - 1) != 0 && blocks < ag_max_blocks {
        blocks += 1;
    }
    blocks
}

pub struct GeomRequest {
    pub devsize: u64,
    pub feat: Features,
    /// -d size=
    pub dsize_blocks: Option<u64>,
    /// -d agcount=
    pub agcount: Option<u64>,
    /// -d agsize= (in blocks)
    pub agsize: Option<u64>,
    /// -l size= (in blocks)
    pub logsize_blocks: Option<u64>,
    /// -l agnum=
    pub logagno: Option<u32>,
}

pub fn compute(req: &GeomRequest) -> Result<Geom, String> {
    let blocksize = 4096u32;
    let blocklog = 12u8;
    let sectorsize = 512u32;
    let sectlog = 9u8;
    let inodesize = 512u32;
    let inodelog = 9u8;

    let mut dblocks = req.devsize / u64::from(blocksize);
    if let Some(d) = req.dsize_blocks {
        if d > dblocks {
            return Err(format!(
                "size {} blocks specified for data subvolume is too large, maximum is {} blocks",
                d, dblocks
            ));
        }
        dblocks = d;
    }
    if dblocks < (300u64 << (20 - 12)) {
        return Err(format!(
            "device is {} blocks, too small for an XFS filesystem with an internal log \
             (need at least 300 MiB)",
            dblocks
        ));
    }

    // AG geometry.
    let (mut agblocks, mut agcount) = match (req.agsize, req.agcount) {
        (Some(sz), _) => (sz, dblocks.div_ceil(sz)),
        (None, Some(cnt)) => (dblocks.div_ceil(cnt), cnt),
        (None, None) => calc_default_ag_geometry(blocklog, dblocks),
    };

    // The last AG is whatever is left over. mkfs refuses a runt below
    // XFS_AG_MIN_BLOCKS; we instead give up the tail so the result is always a
    // valid filesystem rather than an error.
    let ag_min_blocks = XFS_AG_MIN_BYTES >> blocklog;
    let mut last = dblocks - (agcount - 1) * agblocks;
    if agcount > 1 && last < ag_min_blocks {
        dblocks -= last;
        agcount -= 1;
        last = agblocks;
    }
    let _ = last;
    if agcount < 2 {
        // Everything this tool is asked to format is far bigger than this, and
        // an internal log in AG 0 needs mkfs's adjust_ag0_internal_logblocks()
        // dance that we deliberately do not implement.
        return Err("filesystem too small: fewer than 2 allocation groups".into());
    }
    if agblocks > (XFS_AG_MAX_BYTES - 1) >> blocklog {
        return Err(format!("agsize {} blocks too large", agblocks));
    }
    if agblocks < ag_min_blocks {
        return Err(format!("agsize {} blocks too small", agblocks));
    }
    agblocks = agblocks.min(u64::from(u32::MAX));
    agcount = agcount.min(u64::from(u32::MAX));

    let agblklog = ilog2_ceil(agblocks);

    // mkfs: calculate_imaxpct().
    let terablocks = |n: u64| n << (40 - u32::from(blocklog));
    let imaxpct = if dblocks < terablocks(1) {
        XFS_DFL_IMAXIMUM_PCT
    } else if dblocks < terablocks(50) {
        5
    } else {
        1
    };

    // mkfs: calculate_log_size(), internal log, no stripe geometry.
    //   2048:1 fs:log ratio, floored at 64 MiB, capped by what fits in an AG.
    let prealloc = u64::from(req.feat.prealloc_blocks());
    let max_logblocks = agblocks - prealloc - 1;
    let logblocks = match req.logsize_blocks {
        Some(l) => l,
        None => {
            let ratio = dblocks / 2048;
            let floor = 64u64 << (20 - u32::from(blocklog)); // XFS_MIN_REALISTIC_LOG_BLOCKS
            ratio.max(floor).min(max_logblocks)
        }
    };
    if logblocks > max_logblocks {
        return Err(format!(
            "internal log size {} blocks too large, must fit in an allocation group",
            logblocks
        ));
    }
    if logblocks < 512 {
        return Err(format!("internal log size {} blocks too small", logblocks));
    }

    let logagno = req.logagno.unwrap_or((agcount / 2) as u32);
    if u64::from(logagno) >= agcount || logagno == 0 {
        return Err(format!(
            "log ag number {} invalid (must be 1..{})",
            logagno,
            agcount - 1
        ));
    }
    let logstart_agbno = prealloc as u32;
    let logstart_fsb = (u64::from(logagno) << agblklog) | u64::from(logstart_agbno);

    // mkfs: with sparse inodes the cluster alignment moves to spino_align and
    // sb_inoalignmt becomes a full 64-inode chunk.
    let mut cluster_size = XFS_INODE_BIG_CLUSTER_SIZE;
    if inodesize > XFS_DINODE_MIN_SIZE {
        cluster_size *= inodesize / XFS_DINODE_MIN_SIZE;
    }
    let mut inoalignmt = cluster_size >> blocklog;
    let mut spino_align = 0;
    if req.feat.sparse {
        spino_align = inoalignmt;
        inoalignmt = (XFS_INODES_PER_CHUNK * inodesize) >> blocklog;
    }

    Ok(Geom {
        blocksize,
        blocklog,
        sectorsize,
        sectlog,
        inodesize,
        inodelog,
        inopblock: blocksize / inodesize,
        inopblog: blocklog - inodelog,
        dblocks,
        agblocks: agblocks as u32,
        agcount: agcount as u32,
        agblklog,
        imaxpct,
        inoalignmt,
        spino_align,
        logblocks: logblocks as u32,
        logagno,
        logstart_agbno,
        logstart_fsb,
        feat: req.feat,
    })
}

impl Geom {
    /// Blocks in AG `agno` — the last AG holds the remainder.
    pub fn ag_size(&self, agno: u32) -> u32 {
        if agno == self.agcount - 1 {
            (self.dblocks - u64::from(agno) * u64::from(self.agblocks)) as u32
        } else {
            self.agblocks
        }
    }

    /// Byte offset of AG block `agbno` in AG `agno`.
    pub fn ag_byte(&self, agno: u32, agbno: u32) -> u64 {
        (u64::from(agno) * u64::from(self.agblocks) + u64::from(agbno))
            * u64::from(self.blocksize)
    }

    /// 512-byte disk address of AG block `agbno` in AG `agno` (bb_blkno).
    pub fn ag_daddr(&self, agno: u32, agbno: u32) -> u64 {
        self.ag_byte(agno, agbno) / 512
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(bytes: u64) -> Geom {
        compute(&GeomRequest {
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
        .unwrap()
    }

    #[test]
    fn matches_xfsprogs_geometry() {
        // Every expectation below was read off real `mkfs.xfs` 7.1.1 output.
        let x = g(28 << 30);
        assert_eq!(x.dblocks, 7_340_032);
        assert_eq!(x.agcount, 4);
        assert_eq!(x.agblocks, 1_835_008);
        assert_eq!(x.logblocks, 16_384);
        assert_eq!(x.agblklog, 21);
        assert_eq!(x.logstart_fsb, 4_194_311);
        assert_eq!(x.imaxpct, 25);
        assert_eq!(x.inoalignmt, 8);
        assert_eq!(x.spino_align, 4);

        let x = g(1 << 30);
        assert_eq!(x.agcount, 4);
        assert_eq!(x.agblocks, 65_536);
        assert_eq!(x.logblocks, 16_384);

        let x = g(120 << 30);
        assert_eq!(x.agcount, 4);
        assert_eq!(x.agblocks, 7_864_320);
        assert_eq!(x.logblocks, 16_384);

        let x = g(256 << 30);
        assert_eq!(x.agcount, 4);
        assert_eq!(x.logblocks, 32_768);

        let x = g(512 << 30);
        assert_eq!(x.agcount, 4);
        assert_eq!(x.logblocks, 65_536);
    }

    #[test]
    fn prealloc_matches_reference() {
        let f = Features::default();
        assert_eq!(f.prealloc_blocks(), 7);
        assert_eq!(f.rmap_block(), 5);
        assert_eq!(f.refc_block(), 6);
    }
}
