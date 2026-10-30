//! Block-device class (kmod W2-3): the descriptor and ops table a block-driver module registers
//! through `kapi_blk_register`, plus the few scheduler helpers the storage drivers need.
//! Add-only: nothing here changes an existing signature.

/// Disk description. `class` is the Linux name class ("vd", "sd", "nvme", NUL padded); a non-empty
/// `name` names the disk itself ("nvme0n1") instead of `<class><letter>`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KBlkDesc {
    pub size: u32,
    pub sector_size: u32,
    pub nsectors: u64,
    pub removable: u8,
    pub read_only: u8,
    pub class: [u8; 16],
    pub name: [u8; 32],
    pub model: [u8; 48],
}

impl KBlkDesc {
    pub const fn zeroed() -> KBlkDesc {
        KBlkDesc { size: core::mem::size_of::<KBlkDesc>() as u32, sector_size: 512, nsectors: 0, removable: 0,
                   read_only: 0, class: [0; 16], name: [0; 32], model: [0; 48] }
    }
}

/// I/O errors returned by the ops: 0 = ok, otherwise -errno (-5 I/O, -34 range, -22 bad buffer, -19 gone).
pub type KBlkIo = extern "C" fn(ctx: usize, lba: u64, buf: *mut u8, nblk: u32) -> i32;
pub type KBlkFlush = extern "C" fn(ctx: usize) -> i32;
/// Requests in flight, written as (pid, start_ns) pairs into `out`; returns the pair count.
pub type KBlkInflight = extern "C" fn(ctx: usize, out: *mut u64, cap: usize) -> isize;
pub type KBlkWaitNs = extern "C" fn(ctx: usize, pid: u32) -> u64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KBlkOps {
    pub size: u32,
    pub read: KBlkIo,
    pub write: KBlkIo,
    /// Panic-path write: must not wait for a lock another CPU may hold. None = use `write`.
    pub try_write: Option<KBlkIo>,
    pub flush: KBlkFlush,
    pub inflight: Option<KBlkInflight>,
    pub wait_ns: Option<KBlkWaitNs>,
}

pub mod sys_blk {
    use super::*;
    use crate::KmodId;
    pub type BlkRegisterFn = unsafe extern "C" fn(KmodId, *const KBlkDesc, *const KBlkOps, usize) -> i64;
    pub type BlkRemoveFn = unsafe extern "C" fn(i64) -> i32;
    pub type BoolFn = unsafe extern "C" fn() -> bool;
    pub type VoidFn = unsafe extern "C" fn();
    extern "C" {
        pub fn kapi_blk_register(owner: KmodId, desc: *const KBlkDesc, ops: *const KBlkOps, ctx: usize) -> i64;
        pub fn kapi_blk_remove(idx: i64) -> i32;
        pub fn kapi_preempt_disabled() -> bool;
        pub fn kapi_preempt_enable_no_resched();
    }
}
