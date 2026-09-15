//! XFS v5 on-disk constants and structure offsets.
//!
//! Every offset here was read out of xfsprogs 7.1.1 (libxfs/xfs_format.h,
//! libxfs/xfs_da_format.h, libxfs/xfs_log_format.h) and then confirmed byte for
//! byte against an image produced by the real mkfs.xfs 7.1.1. Nothing is
//! guessed.

#![allow(dead_code)]

// ---------------------------------------------------------------- magics ----
pub const XFS_SB_MAGIC: u32 = 0x5846_5342; // "XFSB"
pub const XFS_AGF_MAGIC: u32 = 0x5841_4746; // "XAGF"
pub const XFS_AGI_MAGIC: u32 = 0x5841_4749; // "XAGI"
pub const XFS_AGFL_MAGIC: u32 = 0x5841_464c; // "XAFL"
pub const XFS_DINODE_MAGIC: u16 = 0x494e; // "IN"

pub const XFS_ABTB_CRC_MAGIC: u32 = 0x4142_3342; // "AB3B" bnobt
pub const XFS_ABTC_CRC_MAGIC: u32 = 0x4142_3343; // "AB3C" cntbt
pub const XFS_IBT_CRC_MAGIC: u32 = 0x4941_4233; // "IAB3" inobt
pub const XFS_FIBT_CRC_MAGIC: u32 = 0x4649_4233; // "FIB3" finobt
pub const XFS_RMAP_CRC_MAGIC: u32 = 0x524d_4233; // "RMB3" rmapbt
pub const XFS_REFC_CRC_MAGIC: u32 = 0x5233_4643; // "R3FC" refcountbt

pub const NULLAGBLOCK: u32 = 0xffff_ffff;
pub const NULLAGINO: u32 = 0xffff_ffff;

// ------------------------------------------------- superblock field offsets --
pub const SB_MAGICNUM: usize = 0;
pub const SB_BLOCKSIZE: usize = 4;
pub const SB_DBLOCKS: usize = 8;
pub const SB_RBLOCKS: usize = 16;
pub const SB_REXTENTS: usize = 24;
pub const SB_UUID: usize = 32;
pub const SB_LOGSTART: usize = 48;
pub const SB_ROOTINO: usize = 56;
pub const SB_RBMINO: usize = 64;
pub const SB_RSUMINO: usize = 72;
pub const SB_REXTSIZE: usize = 80;
pub const SB_AGBLOCKS: usize = 84;
pub const SB_AGCOUNT: usize = 88;
pub const SB_RBMBLOCKS: usize = 92;
pub const SB_LOGBLOCKS: usize = 96;
pub const SB_VERSIONNUM: usize = 100;
pub const SB_SECTSIZE: usize = 102;
pub const SB_INODESIZE: usize = 104;
pub const SB_INOPBLOCK: usize = 106;
pub const SB_FNAME: usize = 108; // XFSLABEL_MAX = 12
pub const SB_BLOCKLOG: usize = 120;
pub const SB_SECTLOG: usize = 121;
pub const SB_INODELOG: usize = 122;
pub const SB_INOPBLOG: usize = 123;
pub const SB_AGBLKLOG: usize = 124;
pub const SB_REXTSLOG: usize = 125;
pub const SB_INPROGRESS: usize = 126;
pub const SB_IMAX_PCT: usize = 127;
pub const SB_ICOUNT: usize = 128;
pub const SB_IFREE: usize = 136;
pub const SB_FDBLOCKS: usize = 144;
pub const SB_FREXTENTS: usize = 152;
pub const SB_UQUOTINO: usize = 160;
pub const SB_GQUOTINO: usize = 168;
pub const SB_QFLAGS: usize = 176;
pub const SB_FLAGS: usize = 178;
pub const SB_SHARED_VN: usize = 179;
pub const SB_INOALIGNMT: usize = 180;
pub const SB_UNIT: usize = 184;
pub const SB_WIDTH: usize = 188;
pub const SB_DIRBLKLOG: usize = 192;
pub const SB_LOGSECTLOG: usize = 193;
pub const SB_LOGSECTSIZE: usize = 194;
pub const SB_LOGSUNIT: usize = 196;
pub const SB_FEATURES2: usize = 200;
pub const SB_BAD_FEATURES2: usize = 204;
pub const SB_FEATURES_COMPAT: usize = 208;
pub const SB_FEATURES_RO_COMPAT: usize = 212;
pub const SB_FEATURES_INCOMPAT: usize = 216;
pub const SB_FEATURES_LOG_INCOMPAT: usize = 220;
pub const SB_CRC: usize = 224;
pub const SB_SPINO_ALIGN: usize = 228;
pub const SB_PQUOTINO: usize = 232;
pub const SB_LSN: usize = 240;
pub const SB_META_UUID: usize = 248;
pub const SB_METADIRINO: usize = 264;
pub const SB_RGCOUNT: usize = 272;
pub const SB_RGEXTENTS: usize = 276;
pub const SB_RGBLKLOG: usize = 280;
pub const SB_RTSTART: usize = 288;
pub const SB_RTRESERVED: usize = 296;
pub const SB_SIZE: usize = 304;

// --------------------------------------------------- sb_versionnum bits ------
pub const XFS_SB_VERSION_5: u16 = 5;
pub const XFS_SB_VERSION_ATTRBIT: u16 = 0x0010;
pub const XFS_SB_VERSION_NLINKBIT: u16 = 0x0020;
pub const XFS_SB_VERSION_ALIGNBIT: u16 = 0x0080;
pub const XFS_SB_VERSION_DALIGNBIT: u16 = 0x0100;
pub const XFS_SB_VERSION_LOGV2BIT: u16 = 0x0400;
pub const XFS_SB_VERSION_EXTFLGBIT: u16 = 0x1000;
pub const XFS_SB_VERSION_DIRV2BIT: u16 = 0x2000;
pub const XFS_SB_VERSION_MOREBITSBIT: u16 = 0x8000;

// --------------------------------------------------- sb_features2 bits -------
pub const XFS_SB_VERSION2_LAZYSBCOUNTBIT: u32 = 0x0000_0002;
pub const XFS_SB_VERSION2_ATTR2BIT: u32 = 0x0000_0008;
pub const XFS_SB_VERSION2_PROJID32BIT: u32 = 0x0000_0080;
pub const XFS_SB_VERSION2_CRCBIT: u32 = 0x0000_0100;
pub const XFS_SB_VERSION2_FTYPE: u32 = 0x0000_0200;

// ------------------------------------------------- v5 feature bitmasks -------
pub const XFS_SB_FEAT_RO_COMPAT_FINOBT: u32 = 1 << 0;
pub const XFS_SB_FEAT_RO_COMPAT_RMAPBT: u32 = 1 << 1;
pub const XFS_SB_FEAT_RO_COMPAT_REFLINK: u32 = 1 << 2;
pub const XFS_SB_FEAT_RO_COMPAT_INOBTCNT: u32 = 1 << 3;

pub const XFS_SB_FEAT_INCOMPAT_FTYPE: u32 = 1 << 0;
pub const XFS_SB_FEAT_INCOMPAT_SPINODES: u32 = 1 << 1;
pub const XFS_SB_FEAT_INCOMPAT_META_UUID: u32 = 1 << 2;
pub const XFS_SB_FEAT_INCOMPAT_BIGTIME: u32 = 1 << 3;
pub const XFS_SB_FEAT_INCOMPAT_NREXT64: u32 = 1 << 5;
pub const XFS_SB_FEAT_INCOMPAT_EXCHRANGE: u32 = 1 << 6;
pub const XFS_SB_FEAT_INCOMPAT_PARENT: u32 = 1 << 7;

// ----------------------------------------------------------- AGF / AGI -------
pub const AGF_MAGICNUM: usize = 0;
pub const AGF_VERSIONNUM: usize = 4;
pub const AGF_SEQNO: usize = 8;
pub const AGF_LENGTH: usize = 12;
pub const AGF_BNO_ROOT: usize = 16;
pub const AGF_CNT_ROOT: usize = 20;
pub const AGF_RMAP_ROOT: usize = 24;
pub const AGF_BNO_LEVEL: usize = 28;
pub const AGF_CNT_LEVEL: usize = 32;
pub const AGF_RMAP_LEVEL: usize = 36;
pub const AGF_FLFIRST: usize = 40;
pub const AGF_FLLAST: usize = 44;
pub const AGF_FLCOUNT: usize = 48;
pub const AGF_FREEBLKS: usize = 52;
pub const AGF_LONGEST: usize = 56;
pub const AGF_BTREEBLKS: usize = 60;
pub const AGF_UUID: usize = 64;
pub const AGF_RMAP_BLOCKS: usize = 80;
pub const AGF_REFCOUNT_BLOCKS: usize = 84;
pub const AGF_REFCOUNT_ROOT: usize = 88;
pub const AGF_REFCOUNT_LEVEL: usize = 92;
pub const AGF_LSN: usize = 208;
pub const AGF_CRC: usize = 216;

pub const AGI_MAGICNUM: usize = 0;
pub const AGI_VERSIONNUM: usize = 4;
pub const AGI_SEQNO: usize = 8;
pub const AGI_LENGTH: usize = 12;
pub const AGI_COUNT: usize = 16;
pub const AGI_ROOT: usize = 20;
pub const AGI_LEVEL: usize = 24;
pub const AGI_FREECOUNT: usize = 28;
pub const AGI_NEWINO: usize = 32;
pub const AGI_DIRINO: usize = 36;
pub const AGI_UNLINKED: usize = 40; // 64 x be32
pub const AGI_UUID: usize = 296;
pub const AGI_CRC: usize = 312;
pub const AGI_LSN: usize = 320;
pub const AGI_FREE_ROOT: usize = 328;
pub const AGI_FREE_LEVEL: usize = 332;
pub const AGI_IBLOCKS: usize = 336;
pub const AGI_FBLOCKS: usize = 340;

pub const AGFL_MAGICNUM: usize = 0;
pub const AGFL_SEQNO: usize = 4;
pub const AGFL_UUID: usize = 8;
pub const AGFL_LSN: usize = 24;
pub const AGFL_CRC: usize = 32;
pub const AGFL_HDR_LEN: usize = 36;

// ------------------------------------------------- short-form btree block ----
pub const BB_MAGIC: usize = 0;
pub const BB_LEVEL: usize = 4;
pub const BB_NUMRECS: usize = 6;
pub const BB_LEFTSIB: usize = 8;
pub const BB_RIGHTSIB: usize = 12;
pub const BB_BLKNO: usize = 16;
pub const BB_LSN: usize = 24;
pub const BB_UUID: usize = 32;
pub const BB_OWNER: usize = 48;
pub const BB_CRC: usize = 52;
pub const SBLOCK_CRC_LEN: usize = 56; // header size, records start here

// ------------------------------------------------------- rmap owner codes ----
pub const XFS_RMAP_OWN_FS: u64 = (-3i64) as u64;
pub const XFS_RMAP_OWN_LOG: u64 = (-4i64) as u64;
pub const XFS_RMAP_OWN_AG: u64 = (-5i64) as u64;
pub const XFS_RMAP_OWN_INOBT: u64 = (-6i64) as u64;
pub const XFS_RMAP_OWN_INODES: u64 = (-7i64) as u64;
pub const XFS_RMAP_OWN_REFC: u64 = (-8i64) as u64;

// -------------------------------------------------------------- dinode -------
pub const DI_MAGIC: usize = 0;
pub const DI_MODE: usize = 2;
pub const DI_VERSION: usize = 4;
pub const DI_FORMAT: usize = 5;
pub const DI_METATYPE: usize = 6;
pub const DI_UID: usize = 8;
pub const DI_GID: usize = 12;
pub const DI_NLINK: usize = 16;
pub const DI_PROJID_LO: usize = 20;
pub const DI_PROJID_HI: usize = 22;
pub const DI_BIG_NEXTENTS: usize = 24;
pub const DI_ATIME: usize = 32;
pub const DI_MTIME: usize = 40;
pub const DI_CTIME: usize = 48;
pub const DI_SIZE: usize = 56;
pub const DI_NBLOCKS: usize = 64;
pub const DI_EXTSIZE: usize = 72;
pub const DI_BIG_ANEXTENTS: usize = 76;
pub const DI_FORKOFF: usize = 82;
pub const DI_AFORMAT: usize = 83;
pub const DI_DMEVMASK: usize = 84;
pub const DI_DMSTATE: usize = 88;
pub const DI_FLAGS: usize = 90;
pub const DI_GEN: usize = 92;
pub const DI_NEXT_UNLINKED: usize = 96;
pub const DI_CRC: usize = 100;
pub const DI_CHANGECOUNT: usize = 104;
pub const DI_LSN: usize = 112;
pub const DI_FLAGS2: usize = 120;
pub const DI_COWEXTSIZE: usize = 128;
pub const DI_PAD2: usize = 132;
pub const DI_CRTIME: usize = 144;
pub const DI_INO: usize = 152;
pub const DI_UUID: usize = 160;
pub const DI_LITERAL: usize = 176; // v3 core size; fork data starts here

pub const XFS_DINODE_FMT_DEV: u8 = 0;
pub const XFS_DINODE_FMT_LOCAL: u8 = 1;
pub const XFS_DINODE_FMT_EXTENTS: u8 = 2;

pub const XFS_DIFLAG_NEWRTBM: u16 = 1 << 2;
pub const XFS_DIFLAG2_BIGTIME: u64 = 1 << 3;
pub const XFS_DIFLAG2_NREXT64: u64 = 1 << 4;

pub const S_IFDIR: u16 = 0o040000;
pub const S_IFREG: u16 = 0o100000;

/// Unix epoch -> bigtime epoch offset, in seconds (libxfs: -(int64)S32_MIN).
pub const XFS_BIGTIME_EPOCH_OFFSET: i64 = 2_147_483_648;

// ------------------------------------------------------- shortform attr ------
pub const XFS_ATTR_ROOT: u8 = 1 << 1;

// --------------------------------------------------------------- log ---------
pub const XLOG_HEADER_MAGIC_NUM: u32 = 0xFEED_BABE;
pub const XLOG_FMT_LINUX_LE: u32 = 1;
pub const XLOG_INIT_CYCLE: u32 = 1;
pub const XLOG_BIG_RECORD_BSIZE: u32 = 32 * 1024;
pub const XLOG_UNMOUNT_TYPE: u16 = 0x556e; // "Un"
pub const XLOG_UNMOUNT_TRANS: u8 = 0x20;
pub const XFS_LOG_CLIENT: u8 = 0xaa;
/// Dummy tid mkfs stamps into the unmount record so the kernel can tell the
/// record was written from userspace.
pub const USERSPACE_TID: u32 = 0xb0c0_d0d0;

pub const LOG_H_MAGICNO: usize = 0;
pub const LOG_H_CYCLE: usize = 4;
pub const LOG_H_VERSION: usize = 8;
pub const LOG_H_LEN: usize = 12;
pub const LOG_H_LSN: usize = 16;
pub const LOG_H_TAIL_LSN: usize = 24;
pub const LOG_H_CRC: usize = 32;
pub const LOG_H_PREV_BLOCK: usize = 36;
pub const LOG_H_NUM_LOGOPS: usize = 40;
pub const LOG_H_CYCLE_DATA: usize = 44; // 64 x be32
pub const LOG_H_FMT: usize = 300;
pub const LOG_H_FS_UUID: usize = 304;
pub const LOG_H_SIZE: usize = 320;

pub const BBSIZE: usize = 512;

// --------------------------------------------------- geometry constants ------
pub const XFS_AG_MIN_BYTES: u64 = 1 << 24; // 16 MB
pub const XFS_AG_MAX_BYTES: u64 = 1 << 40; // 1 TB
pub const XFS_MULTIDISK_AGLOG: u32 = 5;
pub const XFS_NOMULTIDISK_AGLOG: u32 = 2;
pub const XFS_DFL_IMAXIMUM_PCT: u8 = 25;
pub const XFS_INODES_PER_CHUNK: u32 = 64;
pub const XFS_INODE_BIG_CLUSTER_SIZE: u32 = 8192;
pub const XFS_DINODE_MIN_SIZE: u32 = 256;
/// mkfs zeroes this much at the head of the device to wipe foreign signatures.
pub const WHACK_SIZE: u64 = 128 * 1024;
