//! Block-device class (kmod W2-3): `kapi_blk_register` wraps a module's ops table in a
//! `BlockDevice` adapter and puts it through the same registry path as a built-in disk (partition
//! scan, `add_disk`); the boot-log partition is re-searched for a disk that shows up late.

use super::*;
use drivers::blockdev::{BlkErr, BlockDevice};
use kmod_abi::{KBlkDesc, KBlkOps};

struct ModDisk {
    ops: KBlkOps,
    ctx: usize,
    ss: u32,
    nsectors: u64,
    removable: bool,
    read_only: bool,
    class: &'static str,
    name: Option<&'static str>,
}

fn cstr(b: &[u8]) -> &[u8] { &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())] }

fn leak_str(b: &[u8]) -> Option<&'static str> {
    let s = core::str::from_utf8(cstr(b)).ok()?;
    if s.is_empty() { None } else { Some(Box::leak(String::from(s).into_boxed_str())) }
}

fn err(r: i32) -> Result<(), BlkErr> {
    match r {
        0 => Ok(()),
        -34 => Err(BlkErr::OutOfRange),
        -22 => Err(BlkErr::BadBuffer),
        -19 => Err(BlkErr::NoDevice),
        _ => Err(BlkErr::Io),
    }
}

impl ModDisk {
    fn io(&self, f: kmod_abi::blk::KBlkIo, lba: u64, p: *mut u8, len: usize) -> Result<(), BlkErr> {
        if len == 0 || len % self.ss as usize != 0 { return Err(BlkErr::BadBuffer); }
        err(f(self.ctx, lba, p, (len / self.ss as usize) as u32))
    }
}

impl BlockDevice for ModDisk {
    fn sector_size(&self) -> u32 { self.ss }
    fn capacity_sectors(&self) -> u64 { self.nsectors }
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkErr> { self.io(self.ops.read, lba, buf.as_mut_ptr(), buf.len()) }
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> { self.io(self.ops.write, lba, buf.as_ptr() as *mut u8, buf.len()) }
    fn try_write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> {
        self.io(self.ops.try_write.unwrap_or(self.ops.write), lba, buf.as_ptr() as *mut u8, buf.len())
    }
    fn flush(&self) -> Result<(), BlkErr> { err((self.ops.flush)(self.ctx)) }
    fn removable(&self) -> bool { self.removable }
    fn name_class(&self) -> &'static str { self.class }
    fn read_only(&self) -> bool { self.read_only }
    fn disk_name(&self) -> Option<&'static str> { self.name }
}

/// Register a disk. Returns its registry index, or a negative errno.
#[no_mangle]
pub extern "C" fn kapi_blk_register(owner: KmodId, desc: *const KBlkDesc, ops: *const KBlkOps, ctx: usize) -> i64 {
    if desc.is_null() || ops.is_null() { return -22; }
    let (d, o) = unsafe { (*desc, *ops) };
    if (d.size as usize) < core::mem::size_of::<KBlkDesc>() || (o.size as usize) < core::mem::size_of::<KBlkOps>() { return -22; }
    let Some(class) = leak_str(&d.class) else { return -22 };
    let md: &'static ModDisk = Box::leak(Box::new(ModDisk {
        ops: o, ctx, ss: d.sector_size, nsectors: d.nsectors, removable: d.removable != 0,
        read_only: d.read_only != 0, class, name: leak_str(&d.name),
    }));
    match drivers::blkdev::register(md) {
        Ok(idx) => {
            crate::kmod::registry::note_registration(owner, RegKind::BLK, idx as u64);
            crate::klog::on_disk_registered();
            idx as i64
        }
        Err(_) => -5,
    }
}

/// Unplug a disk registered through `kapi_blk_register` (its index stays taken, I/O fails).
#[no_mangle]
pub extern "C" fn kapi_blk_remove(idx: i64) -> i32 {
    if idx < 0 { return -22; }
    drivers::blkdev::remove_disk(idx as usize);
    0
}

#[no_mangle]
pub extern "C" fn kapi_preempt_disabled() -> bool { sched::preempt_disabled() }

#[no_mangle]
pub extern "C" fn kapi_preempt_enable_no_resched() { sched::preempt_enable_no_resched() }
