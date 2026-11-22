//! Broadcom V3D 7.1 (Pi 5) as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/v3d/mod.rs`) with `kenv` pointing at the kmod-kapi shim. The scanout sink and
//! `hvs_plane` stay in the kernel. The module is permanent: no `exit`, `RegKind::V3D` is busy forever.
#![no_std]

extern crate alloc;
use core::sync::atomic::{AtomicBool, Ordering};

mod kenv;

#[path = "../../../drivers/src/v3d_ops_abi.rs"]
mod v3d_ops_abi;
/// What `ops.rs` names as `crate::v3d_class::KV3dOps`.
mod v3d_class { pub use crate::v3d_ops_abi::KV3dOps; }

#[path = "../../../drivers/src/v3d/mod.rs"]
#[allow(dead_code)]
mod v3d;

kmod_rt::module! {
    name: "v3d",
    init: init,
    of: ["brcm,2712-v3d"]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverError { NotFound, Io, Unsupported, InvalidParameter, Busy, Errno(i32) }

static STARTED: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if STARTED.swap(true, Ordering::SeqCst) { return -16; }
    // A checkpoint left by an earlier boot stage is not ours.
    let _ = kmod_kapi::plat::serror_checkpoint();
    // Once the hardware was touched the IRQs, the quiesce hook and the MMU exist: never fail then,
    // so the text console and SSH stay.
    let live = v3d::probe_boot(kmod_kapi::cmdline(), 0, kmod_kapi::plat::serror_checkpoint);
    if live { 0 } else { -19 }
}
