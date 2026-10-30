//! kapi exports: platform devices and board services (kmodpi P0). Registrations are per-owner
//! records noted in the registry (`RegKind::PLAT`, `RegKind::DMAQ`) and taken back with the owner.
use super::*;
use alloc::string::String;
use alloc::vec::Vec;
use kmod_abi::{KDmaQuiesceFn, KPlatDev, KPlatProbeFn, KPlatReg};

struct ModPlat { probe: KPlatProbeFn, ctx: usize, compats: Vec<String> }

fn to_k(idx: u32, d: &drivers::platform::PlatDev) -> KPlatDev {
    let mut k = KPlatDev { size: core::mem::size_of::<KPlatDev>() as u32, compat_idx: idx, dma_offset: d.dma_offset,
                           flags: d.flags, dt_node: d.dt_node, ..KPlatDev::default() };
    for i in 0..4 { k.reg[i] = KPlatReg { phys: d.regs[i].0, len: d.regs[i].1 }; }
    if k.reg[0].len == 0 { k.reg[0] = KPlatReg { phys: d.base, len: d.size }; }
    k.irq = d.irqs;
    k.nirq = d.nirq;
    if k.nirq == 0 && d.irq != 0 { k.irq[0] = d.irq; k.nirq = 1; }
    k
}

fn tramp(ctx: usize, compat: &str, d: &drivers::platform::PlatDev) -> bool {
    let m = unsafe { &*(ctx as *const ModPlat) };
    let idx = m.compats.iter().position(|c| c == compat).unwrap_or(0) as u32;
    let k = to_k(idx, d);
    (m.probe)(m.ctx, &k)
}

/// Register a module driver for the NUL-separated `compats`. Every announced unclaimed device it
/// matches is probed before this returns. 0 ok, -22 bad arguments.
#[no_mangle]
pub extern "C" fn kapi_plat_register(owner: KmodId, compats: *const u8, len: usize, probe: KPlatProbeFn, ctx: usize) -> i32 {
    if compats.is_null() || len == 0 || len > 1024 || owner.0 == 0 { return -22; }
    let list: Vec<String> = unsafe { bytes(compats, len) }.split(|&b| b == 0).filter(|c| !c.is_empty())
        .filter_map(|c| core::str::from_utf8(c).ok().map(String::from)).collect();
    if list.is_empty() { return -22; }
    let m = Box::into_raw(Box::new(ModPlat { probe, ctx, compats: list.clone() })) as usize;
    crate::kmod::registry::note_registration(owner, RegKind::PLAT, m as u64);
    drivers::platform::register_ext(owner.0, list, tramp, m);
    0
}

/// Drop every platform driver of `owner` and unclaim its devices. Always 0.
#[no_mangle]
pub extern "C" fn kapi_plat_unregister(owner: KmodId) -> i32 { plat_unregister_owner(owner); 0 }

pub fn plat_unregister_owner(owner: KmodId) {
    for c in drivers::platform::unregister_owner(owner.0) { drop(unsafe { Box::from_raw(c as *mut ModPlat) }); }
}

/// The recorded DTB (null / length 0 when there is none).
#[no_mangle]
pub extern "C" fn kapi_dtb_blob(len: *mut usize) -> *const u8 {
    let b = unsafe { boot::device_tree::recorded_dtb() };
    if !len.is_null() { unsafe { *len = b.map_or(0, |b| b.len()); } }
    b.map_or(core::ptr::null(), |b| b.as_ptr())
}

/// Factory MAC: which 0 eth, 1 wifi, 2 bluetooth. -19 on boards without OTP MACs, -22 bad args.
#[no_mangle]
pub extern "C" fn kapi_board_mac(which: u32, out: *mut u8) -> i32 {
    if out.is_null() || which > 2 { return -22; }
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    {
        let m = match which { 0 => drivers::bcm2712_otp::eth_mac(), 1 => drivers::bcm2712_otp::wifi_mac(), _ => drivers::bcm2712_otp::bt_mac() };
        return match m { Some(m) => { unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), out, 6); } 0 } None => -19 };
    }
    #[allow(unreachable_code)]
    -19
}

/// BCM2712 stepping: 0 C0, 1 D0, 2 unknown; -19 off the Pi 5.
#[no_mangle]
pub extern "C" fn kapi_soc_stepping() -> i32 {
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    {
        use drivers::bcm2712_id::Stepping;
        return match drivers::bcm2712_id::stepping().stepping { Stepping::C0 => 0, Stepping::D0 => 1, Stepping::Unknown => 2 };
    }
    #[allow(unreachable_code)]
    -19
}

/// Consume a pending SError and return its syndrome; 0 when clean (and always off aarch64).
#[no_mangle]
pub extern "C" fn kapi_serror_checkpoint() -> u64 {
    #[cfg(target_arch = "aarch64")]
    { return arch_aarch64::exception::serror_checkpoint().map_or(0, |s| s.max(1)); }
    #[allow(unreachable_code)]
    0
}

/// Contiguous kernel-window alias of `n` physical pages. `cache` 0 UC, 1 WC, 2 WB. 0 on failure.
#[no_mangle]
pub extern "C" fn kapi_vmap(pages: *const u64, n: usize, cache: u32) -> usize {
    if pages.is_null() || n == 0 || n > 4096 || cache > 2 { return 0; }
    let ps = unsafe { core::slice::from_raw_parts(pages, n) };
    if ps.iter().any(|p| p & 0xfff != 0) { return 0; }
    mm::kva::vmap(ps, mm::kva::CacheMode::from_u32(cache)).unwrap_or(0)
}

/// Unmap a `kapi_vmap` alias (the whole mapping goes).
#[no_mangle]
pub extern "C" fn kapi_vunmap(va: usize) { if va != 0 { let _ = mm::kva::vunmap(va); } }

fn quiesce_tramp(ctx: usize) -> bool {
    // ctx = leaked Box<(KDmaQuiesceFn, usize)>; the box lives until the owner's entries are dropped.
    let q = unsafe { &*(ctx as *const (KDmaQuiesceFn, usize)) };
    (q.0)(q.1)
}

static QUIESCE_OWNED: spin::Mutex<Vec<(KmodId, usize, usize)>> = spin::Mutex::new(Vec::new());

/// Register a kexec stop hook for a bus master. `stage` 0 device, 1 bus. -22 bad args, -17 name taken, -12 table full.
/// Runs on every kexec regardless of `KPmOps::quiesce`.
#[no_mangle]
pub extern "C" fn kapi_dma_quiesce_register(owner: KmodId, name: *const u8, len: usize, stage: u32, f: KDmaQuiesceFn, ctx: usize) -> i32 {
    if name.is_null() || len == 0 || len > 31 || stage > 1 || owner.0 == 0 { return -22; }
    let Ok(n) = core::str::from_utf8(unsafe { bytes(name, len) }) else { return -22 };
    if drivers::dma_shutdown::has(n) { return -17; }
    let st = if stage == 0 { drivers::dma_shutdown::Stage::Device } else { drivers::dma_shutdown::Stage::Bus };
    let nm: &'static str = Box::leak(String::from(n).into_boxed_str());
    let q = Box::into_raw(Box::new((f, ctx))) as usize;
    if !drivers::dma_shutdown::register_ext(owner.0, nm, st, quiesce_tramp, q) {
        drop(unsafe { Box::from_raw(q as *mut (KDmaQuiesceFn, usize)) });
        return -12;
    }
    QUIESCE_OWNED.lock().push((owner, q, nm.as_ptr() as usize));
    crate::kmod::registry::note_registration(owner, RegKind::DMAQ, q as u64);
    0
}

/// Failed init / removal: drop every quiesce hook of `owner`.
pub fn dma_quiesce_unregister_owner(owner: KmodId) {
    drivers::dma_shutdown::unregister_owner(owner.0);
    let mut g = QUIESCE_OWNED.lock();
    g.retain(|(o, q, _)| {
        if *o != owner { return true; }
        drop(unsafe { Box::from_raw(*q as *mut (KDmaQuiesceFn, usize)) });
        false
    });
}
