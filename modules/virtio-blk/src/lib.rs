//! virtio-blk as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/virtio_blk.rs`), with `kenv` pointing at the kmod-kapi shim. Its disks go through
//! `kapi_blk_register`. Listed in modules/BOOT so a root on virtio-blk is up before init.
#![no_std]

extern crate alloc;

mod kenv {
    pub use kmod_kapi::blk::env::*;
}

#[path = "../../../drivers/src/blockdev.rs"]
mod blockdev;
#[path = "../../../drivers/src/blkmod.rs"]
mod blkdev;
#[path = "../../../drivers/src/virtio_blk.rs"]
mod virtio_blk;

kmod_rt::module! {
    name: "virtio_blk",
    init: init,
    ids: [kmod_rt::pci(0x1af4, 0x1042), kmod_rt::pci(0x1af4, 0x1001)]
}

fn init() -> i32 {
    let disks = virtio_blk::probe_all();
    if disks.is_empty() { return -19; }
    for d in disks {
        if let Err(e) = blkdev::register(d) {
            kmod_rt::kerr!("blk register failed: {:?}", e);
        }
    }
    0
}
