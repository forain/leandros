//! Module-side `blkdev`: what a storage module sees as `crate::blkdev`. `register` turns a
//! `&'static dyn BlockDevice` into the kapi descriptor + ops table (`kapi_blk_register`); the
//! kernel wraps it in its own adapter and runs the normal registry path. Included only by
//! modules/{nvme,virtio-blk}.

pub use crate::blockdev::{BlkErr, BlockDevice};
use kmod_abi::{KBlkDesc, KBlkOps};

type Dev = &'static dyn BlockDevice;

fn dev(ctx: usize) -> &'static Dev { unsafe { &*(ctx as *const Dev) } }

fn code(r: Result<(), BlkErr>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(BlkErr::OutOfRange) => -34,
        Err(BlkErr::BadBuffer) => -22,
        Err(BlkErr::NoDevice) => -19,
        Err(_) => -5,
    }
}

extern "C" fn read(ctx: usize, lba: u64, buf: *mut u8, nblk: u32) -> i32 {
    let d = dev(ctx);
    let n = nblk as usize * d.sector_size() as usize;
    code(d.read_sectors(lba, unsafe { core::slice::from_raw_parts_mut(buf, n) }))
}
extern "C" fn write(ctx: usize, lba: u64, buf: *mut u8, nblk: u32) -> i32 {
    let d = dev(ctx);
    let n = nblk as usize * d.sector_size() as usize;
    code(d.write_sectors(lba, unsafe { core::slice::from_raw_parts(buf, n) }))
}
extern "C" fn try_write(ctx: usize, lba: u64, buf: *mut u8, nblk: u32) -> i32 {
    let d = dev(ctx);
    let n = nblk as usize * d.sector_size() as usize;
    code(d.try_write_sectors(lba, unsafe { core::slice::from_raw_parts(buf, n) }))
}
extern "C" fn flush(ctx: usize) -> i32 { code(dev(ctx).flush()) }

static OPS: KBlkOps = KBlkOps {
    size: core::mem::size_of::<KBlkOps>() as u32,
    read, write, try_write: Some(try_write), flush, inflight: None, wait_ns: None,
};

fn put(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

/// Register `d` with the kernel block layer; returns its registry index.
pub fn register(d: &'static dyn BlockDevice) -> Result<usize, BlkErr> {
    let mut desc = KBlkDesc::zeroed();
    desc.sector_size = d.sector_size();
    desc.nsectors = d.capacity_sectors();
    desc.removable = d.removable() as u8;
    desc.read_only = d.read_only() as u8;
    put(&mut desc.class, d.name_class());
    if let Some(n) = d.disk_name() { put(&mut desc.name, n); }
    let ctx = alloc::boxed::Box::leak(alloc::boxed::Box::new(d)) as *const Dev as usize;
    kmod_kapi::blk::register(&desc, &OPS, ctx).map(|i| i as usize).map_err(|_| BlkErr::Io)
}
